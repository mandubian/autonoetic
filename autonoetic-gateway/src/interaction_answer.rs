//! Gateway-owned user interaction answer resolution and resume orchestration.
//!
//! See `docs/plan-channel-agnostic-interaction-answering.md`.

use crate::execution::GatewayExecutionService;
use crate::log_redaction::looks_like_secret_value;
use crate::scheduler::workflow_store;
use autonoetic_types::background::{UserInteractionAnswer, UserInteractionStatus};
use autonoetic_types::workflow::TaskRunStatus;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize)]
pub struct InteractionAnswerOutcome {
    pub interaction_id: String,
    pub session_id: Option<String>,
    pub root_session_id: Option<String>,
    pub answer_applied: bool,
    pub resumed: bool,
    pub workflow_task_unblocked: bool,
    pub ambiguous: bool,
    pub ambiguous_candidates: Vec<String>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_reply: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct InteractionAnswerParams {
    pub interaction_id: String,
    #[serde(default)]
    pub answer_text: Option<String>,
    #[serde(default)]
    pub answer_option_id: Option<String>,
    #[serde(default)]
    pub answered_by: Option<String>,
    /// Optional user line appended after injecting the tool result (workflow + standalone).
    #[serde(default)]
    pub follow_up_message: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct InteractionResolveAndAnswerParams {
    /// Strongest correlation: answer this pending interaction.
    #[serde(default)]
    pub interaction_id: Option<String>,
    /// Second priority: inbound reply mapped to the outbound prompt / interaction id.
    #[serde(default)]
    pub reply_to_interaction_id: Option<String>,
    /// Required when resolving without explicit ids: scopes pending set.
    #[serde(default)]
    pub root_session_id: Option<String>,
    #[serde(default)]
    pub answer_text: Option<String>,
    #[serde(default)]
    pub answer_option_id: Option<String>,
    #[serde(default)]
    pub answered_by: Option<String>,
    #[serde(default)]
    pub follow_up_message: Option<String>,
}

fn validate_answer_payload(
    answer_text: &Option<String>,
    answer_option_id: &Option<String>,
) -> anyhow::Result<()> {
    let text = answer_text
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let opt = answer_option_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    anyhow::ensure!(
        text.is_some() ^ opt.is_some(),
        "Provide exactly one of answer_text or answer_option_id"
    );
    Ok(())
}

/// Reject free-text answers for interactions that do not allow freeform.
/// The store also enforces this, but checking before answer orchestration
/// avoids a round-trip and gives callers a clear, immediate error.
fn validate_answer_allows_freeform(
    store: &crate::scheduler::gateway_store::GatewayStore,
    interaction_id: &str,
    answer_text: &Option<String>,
) -> anyhow::Result<()> {
    if answer_text
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return Ok(());
    }
    let interaction = store
        .get_user_interaction(interaction_id)?
        .ok_or_else(|| anyhow::anyhow!("User interaction '{}' not found", interaction_id))?;
    anyhow::ensure!(
        interaction.allow_freeform,
        "Interaction '{}' does not allow freeform answers",
        interaction_id
    );
    Ok(())
}

fn validate_nonsecret_answer_payload(answer_text: &Option<String>) -> anyhow::Result<()> {
    if let Some(text) = answer_text.as_deref() {
        if looks_like_secret_value(text) {
            anyhow::bail!(
                "Secret-like values are not accepted via interaction.answer. Use credential.setup / credential.prompt flow so secrets stay in vault-backed channels."
            );
        }
    }
    Ok(())
}

fn is_critical_divergence_stop_selection(
    interaction: &autonoetic_types::background::UserInteraction,
) -> bool {
    interaction.kind == autonoetic_types::background::UserInteractionKind::DivergenceSentinel
        && interaction.answer_option_id.as_deref() == Some("stop")
}

fn is_divergence_sentinel(
    interaction: &autonoetic_types::background::UserInteraction,
) -> bool {
    interaction.kind == autonoetic_types::background::UserInteractionKind::DivergenceSentinel
}

/// Resolve which pending interaction to answer using deterministic priority.
pub fn resolve_interaction_id(
    store: &crate::scheduler::gateway_store::GatewayStore,
    p: &InteractionResolveAndAnswerParams,
) -> anyhow::Result<Result<String, Vec<String>>> {
    if let Some(id) = p
        .interaction_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(Ok(id.to_string()));
    }
    if let Some(id) = p
        .reply_to_interaction_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(Ok(id.to_string()));
    }
    let root = p
        .root_session_id
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "interaction.resolve_and_answer requires interaction_id, reply_to_interaction_id, or root_session_id"
            )
        })?;
    let pending = store.get_pending_interactions_for_root_session(root)?;
    match pending.len() {
        0 => anyhow::bail!(
            "No pending user interactions for root_session_id '{}'",
            root
        ),
        1 => Ok(Ok(pending[0].interaction_id.clone())),
        _ => Ok(Err(pending
            .iter()
            .map(|i| i.interaction_id.clone())
            .collect())),
    }
}

/// Answer in the store (idempotent) then resume paused workflow tasks or standalone sessions.
pub async fn answer_and_orchestrate_resume(
    execution: &Arc<GatewayExecutionService>,
    params: InteractionAnswerParams,
) -> anyhow::Result<InteractionAnswerOutcome> {
    let cfg = execution.config();
    if !cfg.interaction_answer_orchestration {
        anyhow::bail!("interaction answer orchestration is disabled in gateway config");
    }
    validate_answer_payload(&params.answer_text, &params.answer_option_id)?;

    let store = execution
        .gateway_store()
        .ok_or_else(|| anyhow::anyhow!("GatewayStore required"))?;

    validate_answer_allows_freeform(store.as_ref(), &params.interaction_id, &params.answer_text)?;
    validate_nonsecret_answer_payload(&params.answer_text)?;

    let answered_by = params
        .answered_by
        .clone()
        .unwrap_or_else(|| "gateway".to_string());

    // Idempotent: duplicate deliveries must not re-run resume side-effects.
    if let Some(existing) = store.get_user_interaction(&params.interaction_id)? {
        if existing.status == UserInteractionStatus::Answered {
            return Ok(InteractionAnswerOutcome {
                interaction_id: params.interaction_id.clone(),
                session_id: Some(existing.session_id.clone()),
                root_session_id: Some(existing.root_session_id.clone()),
                answer_applied: false,
                resumed: false,
                workflow_task_unblocked: false,
                ambiguous: false,
                ambiguous_candidates: vec![],
                error: None,
                assistant_reply: None,
            });
        }
    }

    let answer = UserInteractionAnswer {
        interaction_id: params.interaction_id.clone(),
        answer_text: params.answer_text.clone(),
        answer_option_id: params.answer_option_id.clone(),
        answered_by,
    };
    store.answer_user_interaction(&answer)?;

    let interaction = store
        .get_user_interaction(&params.interaction_id)?
        .ok_or_else(|| anyhow::anyhow!("interaction missing after answer"))?;

    // #968: an egress pin×taint ask answered "declassify" materializes the
    // session-wide RemoteModel declassification grant right here — the
    // operator's explicit answer is the authorization, and the resumed turn's
    // routing sees the grant as an already-made decision.
    crate::runtime::egress_labeler::apply_egress_ask_declassification(store.as_ref(), &interaction)?;

    {
        let gateway_dir = crate::execution::gateway_root_dir(cfg.as_ref());
        if let Ok(mut report) = crate::runtime::session_report::SessionReportWriter::open(
            &gateway_dir,
            &interaction.session_id,
            &interaction.agent_id,
        ) {
            let answer_str = params
                .answer_text
                .as_deref()
                .or(params.answer_option_id.as_deref())
                .unwrap_or("—");
            let _ = report.resolve_interaction(&params.interaction_id, answer_str);
        }
    }

    anyhow::ensure!(
        interaction.status == UserInteractionStatus::Answered,
        "unexpected interaction status after apply: {:?}",
        interaction.status
    );

    if is_critical_divergence_stop_selection(&interaction) {
        let reason = format!(
            "Operator selected stop on critical trajectory divergence interaction {} for agent {}",
            interaction.interaction_id, interaction.agent_id
        );
        execution
            .emergency_stop_root_session(
                &interaction.root_session_id,
                &reason,
                "operator",
                params.answered_by.as_deref().unwrap_or("gateway"),
                "interaction.decision.stop",
                None,
            )
            .await?;
        return Ok(InteractionAnswerOutcome {
            interaction_id: params.interaction_id.clone(),
            session_id: Some(interaction.session_id.clone()),
            root_session_id: Some(interaction.root_session_id.clone()),
            answer_applied: true,
            resumed: false,
            workflow_task_unblocked: false,
            ambiguous: false,
            ambiguous_candidates: vec![],
            error: None,
            assistant_reply: None,
        });
    }

    if is_divergence_sentinel(&interaction) {
        tracing::info!(
            target: "interaction",
            interaction_id = %interaction.interaction_id,
            session_id = %interaction.session_id,
            answer = ?interaction.answer_option_id.as_deref().or(interaction.answer_text.as_deref()),
            "Divergence sentinel acknowledged — session continues running"
        );
        let _ = store
            .try_claim_answered_standalone_interaction_resume(&params.interaction_id);
        return Ok(InteractionAnswerOutcome {
            interaction_id: params.interaction_id.clone(),
            session_id: Some(interaction.session_id.clone()),
            root_session_id: Some(interaction.root_session_id.clone()),
            answer_applied: true,
            resumed: false,
            workflow_task_unblocked: false,
            ambiguous: false,
            ambiguous_candidates: vec![],
            error: None,
            assistant_reply: None,
        });
    }

    let follow = params
        .follow_up_message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Workflow-bound: move Paused → Runnable and let durable queue pick it up.
    if let (Some(wf_id), Some(t_id)) = (
        interaction.workflow_id.as_deref(),
        interaction.task_id.as_deref(),
    ) {
        // If the workflow is already terminal, resuming the task is meaningless
        // and can hang the caller (the agent may block on a workflow that will
        // never make progress). Reject the answer with a clear error.
        if workflow_store::is_workflow_terminal(cfg.as_ref(), Some(store.as_ref()), wf_id)? {
            anyhow::bail!(
                "Cannot answer interaction {}: workflow {} is already terminal",
                params.interaction_id,
                wf_id
            );
        }

        let mut unblocked = false;
        if let Some(mut task) =
            workflow_store::load_task_run(cfg.as_ref(), Some(store.as_ref()), wf_id, t_id)?
        {
            if task.status == TaskRunStatus::Paused {
                // Defensive check: only resume if this task was paused for user
                // input, not for child-wait. The checkpoint step distinguishes
                // the two (`paused` vs `paused_child_wait`).
                let is_child_wait = workflow_store::load_task_checkpoint(
                    cfg.as_ref(),
                    Some(store.as_ref()),
                    wf_id,
                    t_id,
                )
                .ok()
                .flatten()
                .map(|cp| cp.step == "paused_child_wait")
                .unwrap_or(false);
                if is_child_wait {
                    // Task is waiting for async children, not user input.
                    // Do not resume; the child terminal transition will wake it.
                    tracing::debug!(
                        target: "interaction",
                        interaction_id = %interaction.interaction_id,
                        task_id = %t_id,
                        "Skipping resume: task is paused for child-wait, not user input"
                    );
                } else {
                    if let Some(fu) = follow {
                        task.message = Some(fu.to_string());
                    }
                    task.updated_at = chrono::Utc::now().to_rfc3339();
                    workflow_store::save_task_run(cfg.as_ref(), Some(store.as_ref()), &task)?;

                    workflow_store::update_task_run_status(
                        cfg.as_ref(),
                        Some(store.as_ref()),
                        wf_id,
                        t_id,
                        TaskRunStatus::Runnable,
                        Some("user interaction answered; resuming task".to_string()),
                        None,
                        None,
                    )?;
                    let _ = workflow_store::checkpoint_task(
                        cfg.as_ref(),
                        Some(store.as_ref()),
                        wf_id,
                        t_id,
                        "user_interaction_answered".to_string(),
                        serde_json::json!({
                            "interaction_id": interaction.interaction_id,
                            "status": "answered",
                        }),
                    );
                    unblocked = true;
                    // Best-effort nudge: the interaction is already persisted as
                    // Answered, so a transient scheduling error here must not
                    // fail the call (a client retry would early-return and never
                    // re-run the nudge). The background tick eventually picks the
                    // Runnable task up regardless.
                    if let Err(e) =
                        crate::scheduler::process_runnable_workflow_tasks(Arc::clone(execution))
                            .await
                    {
                        tracing::warn!(
                            target: "workflow",
                            error = %e,
                            "Failed to process runnable workflow tasks after interaction answer"
                        );
                    }
                }
            }
        }
        // Async `agent_spawn` children are drained by `process_queued_workflow_tasks` (background
        // tick). After an operator answers in chat, nudge the queue immediately so Pending tasks
        // are not left waiting on the next tick (which may be delayed or skipped if a tick fails
        // early while processing background agents).
        if let Err(e) = crate::scheduler::process_queued_workflow_tasks(Arc::clone(execution)).await
        {
            tracing::warn!(
                target: "workflow",
                error = %e,
                "Failed to process queued workflow tasks after workflow-bound interaction answer"
            );
        }
        return Ok(InteractionAnswerOutcome {
            interaction_id: params.interaction_id.clone(),
            session_id: Some(interaction.session_id.clone()),
            root_session_id: Some(interaction.root_session_id.clone()),
            answer_applied: true,
            resumed: unblocked,
            workflow_task_unblocked: unblocked,
            ambiguous: false,
            ambiguous_candidates: vec![],
            error: None,
            assistant_reply: None,
        });
    }

    let default_follow = "[operator] User answered the pending question via interaction.answer.";
    let resume_result = execution
        .resume_session(
            crate::execution::ResumeTrigger::InteractionAnswered {
                interaction_id: params.interaction_id.clone(),
            },
            follow.or(Some(default_follow)),
        )
        .await;

    if let Err(e) = resume_result {
        if let Some(s) = execution.gateway_store() {
            if let Err(release_err) =
                s.release_answered_standalone_interaction_resume_claim(&params.interaction_id)
            {
                tracing::warn!(
                    target: "interaction",
                    interaction_id = %params.interaction_id,
                    error = %release_err,
                    "Failed to release interaction resume claim after resume failure"
                );
            }
        }
        return Err(e);
    }

    let assistant_reply = resume_result
        .ok()
        .and_then(|r| r.assistant_reply);

    if let Err(e) = crate::scheduler::process_queued_workflow_tasks(Arc::clone(execution)).await {
        tracing::warn!(
            target: "workflow",
            error = %e,
            "Failed to process queued workflow tasks after standalone interaction resume"
        );
    }

    Ok(InteractionAnswerOutcome {
        interaction_id: params.interaction_id.clone(),
        session_id: Some(interaction.session_id.clone()),
        root_session_id: Some(interaction.root_session_id.clone()),
        answer_applied: true,
        resumed: true,
        workflow_task_unblocked: false,
        ambiguous: false,
        ambiguous_candidates: vec![],
        error: None,
        assistant_reply,
    })
}

/// Resolve + [`answer_and_orchestrate_resume`].
pub async fn resolve_and_answer(
    execution: &Arc<GatewayExecutionService>,
    params: InteractionResolveAndAnswerParams,
) -> anyhow::Result<InteractionAnswerOutcome> {
    let store = execution
        .gateway_store()
        .ok_or_else(|| anyhow::anyhow!("GatewayStore required"))?;

    match resolve_interaction_id(store.as_ref(), &params)? {
        Ok(id) => {
            answer_and_orchestrate_resume(
                execution,
                InteractionAnswerParams {
                    interaction_id: id,
                    answer_text: params.answer_text,
                    answer_option_id: params.answer_option_id,
                    answered_by: params.answered_by,
                    follow_up_message: params.follow_up_message,
                },
            )
            .await
        }
        Err(candidates) => Ok(InteractionAnswerOutcome {
            interaction_id: String::new(),
            session_id: None,
            root_session_id: None,
            answer_applied: false,
            resumed: false,
            workflow_task_unblocked: false,
            ambiguous: true,
            ambiguous_candidates: candidates,
            error: Some(
                "Multiple pending interactions; specify interaction_id or reply_to_interaction_id"
                    .to_string(),
            ),
            assistant_reply: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_divergence_stop_selection_is_recognized() {
        let interaction = autonoetic_types::background::UserInteraction {
            interaction_id: "ui-1".into(),
            session_id: "session-1".into(),
            root_session_id: "root-1".into(),
            agent_id: "planner.default".into(),
            turn_id: "turn-000005".into(),
            kind: autonoetic_types::background::UserInteractionKind::DivergenceSentinel,
            question: "Critical trajectory divergence in agent 'planner.default' at turn 5.".into(),
            context: None,
            options: vec![
                autonoetic_types::background::UserInteractionOption {
                    id: "ack".into(),
                    label: "Acknowledge".into(),
                    value: "acknowledged".into(),
                },
                autonoetic_types::background::UserInteractionOption {
                    id: "stop".into(),
                    label: "Stop".into(),
                    value: "stop".into(),
                },
            ],
            allow_freeform: true,
            status: UserInteractionStatus::Answered,
            answer_option_id: Some("stop".into()),
            answer_text: None,
            answered_by: Some("chat-tui".into()),
            created_at: "2026-05-20T00:00:00Z".into(),
            answered_at: Some("2026-05-20T00:00:05Z".into()),
            expires_at: None,
            workflow_id: Some("wf-1".into()),
            task_id: Some("task-1".into()),
            checkpoint_turn_id: None,
        };

        assert!(is_critical_divergence_stop_selection(&interaction));
    }
}
