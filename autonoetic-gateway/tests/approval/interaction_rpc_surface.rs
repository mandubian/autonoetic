//! `interaction.list_pending` / `interaction.cancel` service layer (#1119
//! tranche 5) — the RPC surface behind `autonoetic gateway interactions`
//! list/cancel, exercised at service level (no second in-process router, per
//! `tests/session/outcome_rpc.rs` rationale).

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    UserInteraction, UserInteractionAnswer, UserInteractionKind, UserInteractionStatus,
};
use std::sync::Arc;

fn service() -> &'static GatewayExecutionService {
    static SERVICE: std::sync::OnceLock<GatewayExecutionService> = std::sync::OnceLock::new();
    SERVICE.get_or_init(|| {
        let ws = tempfile::tempdir().expect("tempdir");
        let config = autonoetic_types::config::GatewayConfig {
            agents_dir: ws.path().join("agents"),
            ..autonoetic_types::config::GatewayConfig::default()
        };
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        std::mem::forget(ws);
        GatewayExecutionService::new(config, Some(store))
    })
}

fn seed_interaction(session_id: &str) -> UserInteraction {
    UserInteraction {
        interaction_id: format!("ui-{}", uuid::Uuid::new_v4()),
        agent_id: "coder.default".to_string(),
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        turn_id: "turn-1".to_string(),
        kind: UserInteractionKind::Clarification,
        question: "Proceed with the deploy?".to_string(),
        context: None,
        options: Vec::new(),
        allow_freeform: true,
        status: UserInteractionStatus::Pending,
        answer_option_id: None,
        answer_text: None,
        answered_by: None,
        created_at: "2026-08-24T10:00:00+00:00".to_string(),
        answered_at: None,
        expires_at: None,
        workflow_id: None,
        task_id: None,
        checkpoint_turn_id: None,
    }
}

#[tokio::test]
async fn list_pending_scopes_by_session_and_root() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let i1 = seed_interaction("root-a");
    store.create_user_interaction(&i1).expect("seed");

    let by_root = svc
        .pending_user_interactions(Some("root-a"), None)
        .expect("root-scoped");
    assert!(by_root.iter().any(|i| i.interaction_id == i1.interaction_id));

    let by_session = svc
        .pending_user_interactions(None, Some("root-a"))
        .expect("session-scoped");
    assert!(by_session.iter().any(|i| i.interaction_id == i1.interaction_id));

    let empty = svc
        .pending_user_interactions(Some("root-nope"), None)
        .expect("no matches");
    assert!(empty.is_empty());
}

#[tokio::test]
async fn list_pending_rejects_ambiguous_scopes() {
    let err = service()
        .pending_user_interactions(None, None)
        .expect_err("must require a scope");
    assert!(err.to_string().contains("exactly one"));
}

#[tokio::test]
async fn cancel_marks_cancelled_with_reason() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let i = seed_interaction("root-cancel");
    store.create_user_interaction(&i).expect("seed");

    svc.cancel_user_interaction(&i.interaction_id, "operator abort")
        .expect("cancel");

    let got = store
        .get_user_interaction(&i.interaction_id)
        .expect("get")
        .expect("present");
    assert_eq!(got.status, UserInteractionStatus::Cancelled);
    // cancel_user_interaction records the reason in the row's answer_text.
    assert_eq!(
        store
            .get_user_interaction(&i.interaction_id)
            .expect("re-get")
            .expect("present")
            .answer_text,
        Some("operator abort".to_string())
    );
}

/// The answer path used by the CLI now is the canonical resume-orchestration
/// RPC (interaction.answer); this pins that the single-row write underneath
/// it still behaves end-to-end for a plain record.
#[tokio::test]
async fn plain_answer_records_text_and_status() {
    let svc = service();
    let store = svc.gateway_store().expect("store");
    let i = seed_interaction("root-answer");
    store.create_user_interaction(&i).expect("seed");

    let answer = UserInteractionAnswer {
        interaction_id: i.interaction_id.clone(),
        answer_text: Some("yes".to_string()),
        answer_option_id: None,
        answered_by: "cli".to_string(),
    };
    store.answer_user_interaction(&answer).expect("answer");

    let got = store
        .get_user_interaction(&i.interaction_id)
        .expect("get")
        .expect("present");
    assert_eq!(got.status, UserInteractionStatus::Answered);
    assert_eq!(got.answer_text.as_deref(), Some("yes"));
}