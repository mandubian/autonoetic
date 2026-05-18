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
    validate_nonsecret_answer_payload(&params.answer_text)?;

    let store = execution
        .gateway_store()
        .ok_or_else(|| anyhow::anyhow!("GatewayStore required"))?;

    let answered_by = params
        .answered_by
        .clone()
        .unwrap_or_else(|| "gateway".to_string());

    // Idempotent: duplicate deliveries must not re-run resume side-effects.
    if let Some(existing) = store.get_user_interaction(&params.interaction_id)? {
        if existing.status == UserInteractionStatus::Answered {
            return Ok(InteractionAnswerOutcome {
                interaction_id: params.interaction_id.clone(),
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
        let mut unblocked = false;
        if let Some(mut task) =
            workflow_store::load_task_run(cfg.as_ref(), Some(store.as_ref()), wf_id, t_id)?
        {
            if task.status == TaskRunStatus::Paused {
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
                crate::scheduler::process_runnable_workflow_tasks(Arc::clone(execution)).await?;
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
        .resume_from_user_interaction(&params.interaction_id, follow.or(Some(default_follow)))
        .await;

    if let Err(e) = resume_result {
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
