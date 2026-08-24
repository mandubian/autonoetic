//! #741: the single resume entrypoint verifies trigger/YieldReason coherence.
//!
//! Pins the behaviors that are load-bearing for the scheduler and operators:
//! - the machine-matched `session_waiting_for_approval:<session>:<rid>` error
//!   (scheduler.rs matches this prefix to defer standalone-interaction
//!   resumes) survives the #741 refactor verbatim;
//! - an `ApprovalResolved` trigger cannot resume a session parked on a
//!   *different* approval;
//! - emergency-stopped checkpoints refuse every trigger (R-6.14).

use autonoetic_gateway::execution::{GatewayExecutionService, ResumeTrigger};
use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{save_checkpoint, SessionCheckpoint, YieldReason};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    ApprovalLevel, ApprovalRequest, ScheduledAction, UserInteraction, UserInteractionKind,
    UserInteractionStatus,
};
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;

fn setup() -> (tempfile::TempDir, GatewayConfig, Arc<GatewayStore>) {
    let temp = tempfile::tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let config = GatewayConfig {
        runtime_dir: gateway_dir.clone(),
        agents_dir,
        ..Default::default()
    };
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());
    (temp, config, store)
}

fn checkpoint(session_id: &str, yield_reason: YieldReason) -> SessionCheckpoint {
    SessionCheckpoint {
        egress_labels: Default::default(),
        egress_ask: None,
        history: vec![Message::system("sys"), Message::user("hi")],
        turn_counter: 1,
        session_state: Default::default(),
        tool_tier_escalated: false,
        session_phase: Default::default(),
                discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        extended_loaded: false,
        loop_guard_state: LoopGuard::default(),
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: "turn-1".to_string(),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason,
        content_store_refs: vec![],
        created_at: chrono::Utc::now().to_rfc3339(),
        pending_tool_state: None,
        llm_rounds_consumed: 1,
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
    }
}

fn answered_interaction(session_id: &str, interaction_id: &str) -> UserInteraction {
    UserInteraction {
        interaction_id: interaction_id.to_string(),
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        workflow_id: None,
        task_id: None,
        agent_id: "test-agent".to_string(),
        turn_id: "turn-1".to_string(),
        kind: UserInteractionKind::Clarification,
        question: "Which region?".to_string(),
        context: None,
        options: vec![],
        allow_freeform: true,
        status: UserInteractionStatus::Answered,
        answer_option_id: None,
        answer_text: Some("eu-west".to_string()),
        answered_by: Some("operator".to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
        answered_at: Some(chrono::Utc::now().to_rfc3339()),
        expires_at: None,
        checkpoint_turn_id: None,
    }
}

/// The scheduler machine-matches this prefix (scheduler.rs) to defer resumes
/// of interactions whose session has moved on to an approval gate. The #741
/// refactor must preserve it verbatim.
#[tokio::test]
async fn interaction_resume_preserves_waiting_for_approval_error() {
    let (_t, config, store) = setup();
    let session_id = "root-741/child-a";

    store
        .create_user_interaction(&answered_interaction(session_id, "ui-741"))
        .unwrap();
    // create_user_interaction stores it Pending regardless of the struct's
    // status; answer through the real store path so status becomes Answered.
    store
        .answer_user_interaction(&autonoetic_types::background::UserInteractionAnswer {
            interaction_id: "ui-741".to_string(),
            answer_option_id: None,
            answer_text: Some("eu-west".to_string()),
            answered_by: "operator".to_string(),
        })
        .unwrap();
    save_checkpoint(
        &config,
        &checkpoint(
            session_id,
            YieldReason::ApprovalRequired {
                approval_request_id: "apr-741".to_string(),
            },
        ),
    )
    .unwrap();

    let svc = GatewayExecutionService::new(config, Some(store));
    let err = svc
        .resume_session(
            ResumeTrigger::InteractionAnswered {
                interaction_id: "ui-741".to_string(),
            },
            None,
        )
        .await
        .expect_err("must refuse: session moved on to an approval gate");

    assert_eq!(
        err.to_string(),
        format!("session_waiting_for_approval:{}:apr-741", session_id),
        "machine-matched error prefix must survive the #741 refactor verbatim"
    );
}

/// An ApprovalResolved trigger for approval A must not resume a session whose
/// checkpoint is parked on approval B.
#[tokio::test]
async fn approval_trigger_rejects_mismatched_checkpoint() {
    let (_t, config, store) = setup();
    let session_id = "root-741/child-b";

    let mut req = ApprovalRequest {
        request_id: "apr-A".to_string(),
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        action: ScheduledAction::WriteFile {
            path: "/tmp/x".to_string(),
            content: "x".to_string(),
            requires_approval: true,
            evidence_ref: None,
        },
        approval_level: ApprovalLevel::Operator,
        created_at: chrono::Utc::now().to_rfc3339(),
        reason: None,
        evidence_ref: None,
        workflow_id: None,
        task_id: None,
        root_session_id: Some("root-741".to_string()),
        status: None,
        decided_at: None,
        decided_by: None,
        decision_reason: None,
        min_dwell_ms: None,
        confirm_phrase: None,
        code_excerpts: None,
        risk_summary: None,
        expires_at: None,
    };
    store.create_approval(&mut req).unwrap();

    save_checkpoint(
        &config,
        &checkpoint(
            session_id,
            YieldReason::ApprovalRequired {
                approval_request_id: "apr-B".to_string(),
            },
        ),
    )
    .unwrap();

    let svc = GatewayExecutionService::new(config, Some(store));
    let err = svc
        .resume_session(
            ResumeTrigger::ApprovalResolved {
                request_id: "apr-A".to_string(),
            },
            None,
        )
        .await
        .expect_err("must refuse: checkpoint is parked on a different approval");
    let msg = err.to_string();
    assert!(
        msg.contains("apr-B") && msg.contains("apr-A"),
        "error should name both approvals: {msg}"
    );
}

/// Emergency-stopped checkpoints refuse every trigger, including Manual
/// (R-6.14: no auto-resume after an emergency stop).
#[tokio::test]
async fn manual_resume_refuses_emergency_stopped_checkpoint() {
    let (_t, config, store) = setup();
    let session_id = "root-741/child-c";

    save_checkpoint(
        &config,
        &checkpoint(
            session_id,
            YieldReason::EmergencyStop {
                stop_id: "stop-1".to_string(),
            },
        ),
    )
    .unwrap();

    let svc = GatewayExecutionService::new(config, Some(store));
    let err = svc
        .resume_session(
            ResumeTrigger::Manual {
                session_id: session_id.to_string(),
            },
            None,
        )
        .await
        .expect_err("must refuse: emergency-stopped sessions are never auto-resumed");
    assert!(
        err.to_string().contains("never auto-resumed"),
        "error should state the R-6.14 rule: {}",
        err
    );
}
