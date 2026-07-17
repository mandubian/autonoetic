//! Session resume helpers for checkpoint and user interaction resume paths.

use crate::llm::Message;
use autonoetic_types::background::{UserInteraction, UserInteractionStatus};

/// What woke a suspended session (#741). The single resume entrypoint
/// (`GatewayExecutionService::resume_session`) verifies the trigger is
/// coherent with the checkpoint's `YieldReason` before reconstructing —
/// resuming a `UserInputRequired` checkpoint because an *approval* resolved
/// (or vice versa) is a routing bug that previously surfaced only as
/// downstream re-suspends or hangs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeTrigger {
    /// An operator/decider resolved the approval the session is parked on.
    ApprovalResolved { request_id: String },
    /// The user interaction the session asked was answered.
    InteractionAnswered { interaction_id: String },
    /// Operator-driven or recovery resume with no specific gate resolution
    /// (hibernation wake, crash recovery, manual restart).
    Manual { session_id: String },
}

/// Why a trigger is not coherent with the checkpoint it would resume.
/// The caller maps these to its legacy error strings — notably
/// `WaitingForApproval`, whose `session_waiting_for_approval:<session>:<rid>`
/// rendering is machine-matched by the scheduler's standalone-interaction
/// sweep and must not change shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TriggerIncoherence {
    /// Checkpoint waits on a different interaction than the trigger's.
    InteractionMismatch { expected: String, got: String },
    /// Checkpoint waits on a different approval than the trigger's.
    ApprovalMismatch { expected: String, got: String },
    /// An interaction trigger arrived but the session has since moved on to
    /// an approval gate.
    WaitingForApproval { request_id: String },
    /// The checkpoint's yield reason is not one this trigger can resume.
    WrongYieldReason { got: String },
    /// Emergency-stopped sessions are never auto-resumed (R-6.14).
    EmergencyStop,
}

/// Pure trigger-vs-checkpoint coherence check (#741). `Ok(())` means the
/// trigger may resume a session whose latest checkpoint has `yield_reason`.
/// Encodes exactly the checks the pre-#741 paths performed ad hoc:
/// - the interaction path's "checkpoint is for interaction X, not Y" /
///   "session now waiting for approval" / "not UserInputRequired" guards;
/// - the universal EmergencyStop no-auto-resume rule;
/// - `Manual` accepts anything except EmergencyStop (matching
///   `spawn_agent_once`'s behavior of delegating every reason to
///   `resume_from_checkpoint`).
pub(crate) fn verify_trigger_coherence(
    trigger: &ResumeTrigger,
    yield_reason: &crate::runtime::checkpoint::YieldReason,
) -> Result<(), TriggerIncoherence> {
    use crate::runtime::checkpoint::YieldReason;

    if matches!(yield_reason, YieldReason::EmergencyStop { .. }) {
        return Err(TriggerIncoherence::EmergencyStop);
    }

    match trigger {
        ResumeTrigger::Manual { .. } => Ok(()),
        ResumeTrigger::InteractionAnswered { interaction_id } => match yield_reason {
            YieldReason::UserInputRequired { interaction_id: cid } => {
                if cid == interaction_id {
                    Ok(())
                } else {
                    Err(TriggerIncoherence::InteractionMismatch {
                        expected: cid.clone(),
                        got: interaction_id.clone(),
                    })
                }
            }
            YieldReason::ApprovalRequired {
                approval_request_id,
            } => Err(TriggerIncoherence::WaitingForApproval {
                request_id: approval_request_id.clone(),
            }),
            other => Err(TriggerIncoherence::WrongYieldReason {
                got: format!("{other:?}"),
            }),
        },
        ResumeTrigger::ApprovalResolved { request_id } => match yield_reason {
            YieldReason::ApprovalRequired {
                approval_request_id,
            } => {
                if approval_request_id == request_id {
                    Ok(())
                } else {
                    Err(TriggerIncoherence::ApprovalMismatch {
                        expected: approval_request_id.clone(),
                        got: request_id.clone(),
                    })
                }
            }
            other => Err(TriggerIncoherence::WrongYieldReason {
                got: format!("{other:?}"),
            }),
        },
    }
}

pub(crate) fn should_auto_resume_checkpoint_yield_reason(
    yield_reason: &crate::runtime::checkpoint::YieldReason,
) -> bool {
    use crate::runtime::checkpoint::YieldReason;
    matches!(
        yield_reason,
        YieldReason::Hibernation
            | YieldReason::BudgetExhausted
            | YieldReason::WaitingForChild { .. }
            | YieldReason::ManualStop
            | YieldReason::Error(_)
    )
}

pub(crate) fn build_user_ask_answer_tool_result_json(interaction: &UserInteraction) -> anyhow::Result<String> {
    if interaction.status != UserInteractionStatus::Answered {
        anyhow::bail!(
            "user interaction {} is not answered ({:?})",
            interaction.interaction_id,
            interaction.status
        );
    }
    let selected_value = match &interaction.answer_option_id {
        Some(oid) => interaction
            .options
            .iter()
            .find(|o| &o.id == oid)
            .map(|o| o.value.clone()),
        None => None,
    };
    Ok(serde_json::json!({
        "ok": true,
        "interaction_id": interaction.interaction_id,
        "status": "answered",
        "question": interaction.question,
        "kind": interaction.kind.as_str(),
        "answer_text": interaction.answer_text,
        "answer_option_id": interaction.answer_option_id,
        "selected_value": selected_value,
    })
    .to_string())
}

pub(crate) fn resolve_pending_user_ask_call(
    checkpoint: &crate::runtime::checkpoint::SessionCheckpoint,
) -> anyhow::Result<(String, String)> {
    if let Some(ref pts) = checkpoint.pending_tool_state {
        return Ok((
            pts.pending_tool_call.call_id.clone(),
            pts.pending_tool_call.tool_name.clone(),
        ));
    }
    pending_user_ask_call_from_history(&checkpoint.history)
}

pub(crate) fn pending_user_ask_call_from_history(history: &[Message]) -> anyhow::Result<(String, String)> {
    use crate::llm::Role;
    let i = history
        .iter()
        .rposition(|m| matches!(m.role, Role::Assistant) && !m.tool_calls.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("checkpoint history has no assistant message with tool calls")
        })?;
    let assistant = &history[i];
    let mut j = i + 1;
    let mut tc_idx = 0usize;
    while tc_idx < assistant.tool_calls.len() && j < history.len() {
        let m = &history[j];
        if matches!(m.role, Role::Tool)
            && m.tool_call_id.as_deref() == Some(assistant.tool_calls[tc_idx].id.as_str())
        {
            tc_idx += 1;
            j += 1;
        } else {
            break;
        }
    }
    if tc_idx >= assistant.tool_calls.len() {
        anyhow::bail!("checkpoint history has no pending tool call (batch missing result)");
    }
    let tc = &assistant.tool_calls[tc_idx];
    if tc.name != "user_ask" {
        anyhow::bail!(
            "expected pending tool user_ask for UserInputRequired checkpoint, found {}",
            tc.name
        );
    }
    Ok((tc.id.clone(), tc.name.clone()))
}

pub(crate) fn inject_answered_user_interaction_into_history(
    history: &mut Vec<Message>,
    checkpoint: &crate::runtime::checkpoint::SessionCheckpoint,
    interaction: &UserInteraction,
) -> anyhow::Result<()> {
    let (call_id, tool_name) = resolve_pending_user_ask_call(checkpoint)?;
    let json = build_user_ask_answer_tool_result_json(interaction)?;
    history.push(Message::tool_result(call_id, tool_name, json));
    Ok(())
}

#[cfg(test)]
mod session_resume_tests {
    use super::*;

    #[test]
    fn pending_user_ask_call_from_history_finds_first_missing_result() {
        use crate::llm::ToolCall;
        let mut a = Message::assistant("");
        a.tool_calls = vec![
            ToolCall {
                id: "c1".into(),
                name: "noop".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "c2".into(),
                name: "user_ask".into(),
                arguments: "{}".into(),
            },
        ];
        let history = vec![
            Message::user("hi"),
            a,
            Message::tool_result("c1", "noop", r#"{"ok":true}"#),
        ];
        let (id, name) = pending_user_ask_call_from_history(&history).unwrap();
        assert_eq!(id, "c2");
        assert_eq!(name, "user_ask");
    }

    #[test]
    fn resolve_pending_prefers_checkpoint_pending_tool_state() {
        use crate::runtime::checkpoint::{
            PendingToolCall, PendingToolState, SessionCheckpoint, YieldReason,
        };
        use crate::runtime::guard::LoopGuard;
        let pts = PendingToolState {
            completed_tool_results: vec![],
            pending_tool_call: PendingToolCall {
                call_id: "tid-99".into(),
                tool_name: "user_ask".into(),
                arguments: "{}".into(),
                approval_response: None,
            },
            remaining_tool_calls: vec![],
        };
        let cp = SessionCheckpoint {
            history: vec![],
            turn_counter: 0,
            loop_guard_state: LoopGuard {
                max_loops_without_progress: 1,
                max_tool_failures: 5,
                max_consecutive_same_progress: 0,
                max_child_failures: 3,
                current_loops: 0,
                tool_failure_counts: std::collections::HashMap::new(),
                last_progress_fingerprint: None,
                consecutive_progress_count: 0,
                child_failure_count: 0,
                ..Default::default()
            },
            session_state: autonoetic_types::agent::SessionState::Normal,
            tool_tier_escalated: false,
            discovered_tools: Default::default(),
            blocked_state_event_emitted: false,
            agent_id: "a".into(),
            session_id: "s".into(),
            turn_id: "turn-1".into(),
            workflow_id: None,
            task_id: None,
            runtime_lock_hash: None,
            constitution_version: None,
            constitution_digest: None,
            llm_config_snapshot: None,
            tool_registry_version: None,
            yield_reason: YieldReason::UserInputRequired {
                interaction_id: "ui-x".into(),
            },
            content_store_refs: vec![],
            created_at: "".into(),
            pending_tool_state: Some(pts),
            llm_rounds_consumed: 0,
            tool_invocations_consumed: 0,
            tokens_consumed: 0,
            estimated_cost_usd: 0.0,
            compression_metadata: None,
            capsule_state: None,
            assistant_message: None,
            pending_action: None,
            suspended_at: None,
            suppress_until_turn: 0,
            trajectory_last_level: None,
            feedback_events: vec![],
        };
        let (id, name) = resolve_pending_user_ask_call(&cp).unwrap();
        assert_eq!(id, "tid-99");
        assert_eq!(name, "user_ask");
    }

    #[test]
    fn build_user_ask_answer_includes_selected_value() {
        use autonoetic_types::background::{
            UserInteraction, UserInteractionKind, UserInteractionStatus,
        };
        let interaction = UserInteraction {
            interaction_id: "ui-abc".into(),
            session_id: "s1".into(),
            root_session_id: "s1".into(),
            agent_id: "ag1".into(),
            turn_id: "t1".into(),
            kind: UserInteractionKind::Decision,
            question: "Pick one".into(),
            context: None,
            options: vec![autonoetic_types::background::UserInteractionOption {
                id: "opt-a".into(),
                label: "A".into(),
                value: "alpha".into(),
            }],
            allow_freeform: false,
            status: UserInteractionStatus::Answered,
            answer_option_id: Some("opt-a".into()),
            answer_text: None,
            answered_by: Some("cli".into()),
            created_at: "".into(),
            answered_at: None,
            expires_at: None,
            workflow_id: None,
            task_id: None,
            checkpoint_turn_id: None,
        };
        let json = build_user_ask_answer_tool_result_json(&interaction).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["selected_value"], "alpha");
        assert_eq!(v["answer_option_id"], "opt-a");
    }

    #[test]
    fn waiting_for_child_checkpoint_is_auto_resumable() {
        let resumable = crate::runtime::checkpoint::YieldReason::WaitingForChild {
            workflow_id: "wf-123".into(),
            task_id: None,
        };
        assert!(should_auto_resume_checkpoint_yield_reason(&resumable));
    }

    // ── #741: trigger/YieldReason coherence ─────────────────────────────

    fn approval_reason(rid: &str) -> crate::runtime::checkpoint::YieldReason {
        crate::runtime::checkpoint::YieldReason::ApprovalRequired {
            approval_request_id: rid.to_string(),
        }
    }

    fn interaction_reason(id: &str) -> crate::runtime::checkpoint::YieldReason {
        crate::runtime::checkpoint::YieldReason::UserInputRequired {
            interaction_id: id.to_string(),
        }
    }

    #[test]
    fn coherence_interaction_trigger_matches_its_checkpoint() {
        let t = ResumeTrigger::InteractionAnswered {
            interaction_id: "ui-1".into(),
        };
        assert_eq!(verify_trigger_coherence(&t, &interaction_reason("ui-1")), Ok(()));
        assert_eq!(
            verify_trigger_coherence(&t, &interaction_reason("ui-2")),
            Err(TriggerIncoherence::InteractionMismatch {
                expected: "ui-2".into(),
                got: "ui-1".into(),
            })
        );
    }

    #[test]
    fn coherence_interaction_trigger_detects_session_moved_to_approval() {
        let t = ResumeTrigger::InteractionAnswered {
            interaction_id: "ui-1".into(),
        };
        assert_eq!(
            verify_trigger_coherence(&t, &approval_reason("apr-9")),
            Err(TriggerIncoherence::WaitingForApproval {
                request_id: "apr-9".into(),
            })
        );
    }

    #[test]
    fn coherence_approval_trigger_matches_its_checkpoint() {
        let t = ResumeTrigger::ApprovalResolved {
            request_id: "apr-1".into(),
        };
        assert_eq!(verify_trigger_coherence(&t, &approval_reason("apr-1")), Ok(()));
        assert_eq!(
            verify_trigger_coherence(&t, &approval_reason("apr-2")),
            Err(TriggerIncoherence::ApprovalMismatch {
                expected: "apr-2".into(),
                got: "apr-1".into(),
            })
        );
        assert!(matches!(
            verify_trigger_coherence(&t, &interaction_reason("ui-1")),
            Err(TriggerIncoherence::WrongYieldReason { .. })
        ));
    }

    #[test]
    fn coherence_emergency_stop_refuses_every_trigger() {
        let es = crate::runtime::checkpoint::YieldReason::EmergencyStop {
            stop_id: "stop-1".into(),
        };
        for t in [
            ResumeTrigger::Manual {
                session_id: "s".into(),
            },
            ResumeTrigger::ApprovalResolved {
                request_id: "apr-1".into(),
            },
            ResumeTrigger::InteractionAnswered {
                interaction_id: "ui-1".into(),
            },
        ] {
            assert_eq!(
                verify_trigger_coherence(&t, &es),
                Err(TriggerIncoherence::EmergencyStop),
                "{t:?} must not resume an emergency-stopped session"
            );
        }
    }

    #[test]
    fn coherence_manual_accepts_everything_but_emergency_stop() {
        let t = ResumeTrigger::Manual {
            session_id: "s".into(),
        };
        for reason in [
            crate::runtime::checkpoint::YieldReason::Hibernation,
            crate::runtime::checkpoint::YieldReason::BudgetExhausted,
            approval_reason("apr-1"),
            interaction_reason("ui-1"),
        ] {
            assert_eq!(verify_trigger_coherence(&t, &reason), Ok(()));
        }
    }
}
