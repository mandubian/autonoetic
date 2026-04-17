//! Resolution priority for `interaction.resolve_and_answer` (root_session_id scoping).

use autonoetic_gateway::interaction_answer::resolve_interaction_id;
use autonoetic_gateway::interaction_answer::InteractionResolveAndAnswerParams;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{
    UserInteraction, UserInteractionKind, UserInteractionStatus,
};
use tempfile::tempdir;

fn minimal_interaction(id: &str, root: &str, session: &str) -> UserInteraction {
    let now = chrono::Utc::now().to_rfc3339();
    UserInteraction {
        interaction_id: id.to_string(),
        session_id: session.to_string(),
        root_session_id: root.to_string(),
        agent_id: "agent.test".to_string(),
        turn_id: "t1".to_string(),
        kind: UserInteractionKind::Clarification,
        question: "q?".to_string(),
        context: None,
        options: vec![],
        allow_freeform: true,
        status: UserInteractionStatus::Pending,
        answer_option_id: None,
        answer_text: None,
        answered_by: None,
        created_at: now.clone(),
        answered_at: None,
        expires_at: None,
        workflow_id: None,
        task_id: None,
        checkpoint_turn_id: None,
    }
}

#[test]
fn resolve_prefers_explicit_interaction_id() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gw = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gw)?;
    let store = GatewayStore::open(&gw)?;

    let p = InteractionResolveAndAnswerParams {
        interaction_id: Some("ui-explicit".to_string()),
        reply_to_interaction_id: Some("ui-reply".to_string()),
        root_session_id: Some("root".to_string()),
        answer_text: Some("a".to_string()),
        answer_option_id: None,
        answered_by: None,
        follow_up_message: None,
    };
    let resolved = resolve_interaction_id(&store, &p)?;
    assert_eq!(resolved.unwrap(), "ui-explicit");
    Ok(())
}

#[test]
fn resolve_single_pending_under_root() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gw = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gw)?;
    let store = GatewayStore::open(&gw)?;

    store.create_user_interaction(&minimal_interaction(
        "ui-only",
        "root-1",
        "root-1/sess",
    ))?;

    let p = InteractionResolveAndAnswerParams {
        interaction_id: None,
        reply_to_interaction_id: None,
        root_session_id: Some("root-1".to_string()),
        answer_text: Some("ok".to_string()),
        answer_option_id: None,
        answered_by: None,
        follow_up_message: None,
    };
    let resolved = resolve_interaction_id(&store, &p)?;
    assert_eq!(resolved.unwrap(), "ui-only");
    Ok(())
}

#[test]
fn resolve_ambiguous_when_multiple_pending() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let gw = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gw)?;
    let store = GatewayStore::open(&gw)?;

    store.create_user_interaction(&minimal_interaction(
        "ui-a",
        "root-2",
        "root-2/s1",
    ))?;
    store.create_user_interaction(&minimal_interaction(
        "ui-b",
        "root-2",
        "root-2/s2",
    ))?;

    let p = InteractionResolveAndAnswerParams {
        interaction_id: None,
        reply_to_interaction_id: None,
        root_session_id: Some("root-2".to_string()),
        answer_text: Some("ok".to_string()),
        answer_option_id: None,
        answered_by: None,
        follow_up_message: None,
    };
    let resolved = resolve_interaction_id(&store, &p)?;
    let amb = resolved.unwrap_err();
    assert_eq!(amb.len(), 2);
    Ok(())
}
