//! Integration tests for the `agent.spawn` hook action.
//!
//! These tests exercise `HookExecutor` directly with a real tokio runtime and a
//! real mpsc channel, verifying that the correct `HookSpawnRequest` values are
//! delivered (or withheld) for each scenario.

use std::collections::HashMap;

use autonoetic_gateway::scheduler::hooks::{HookExecutor, HookSpawnRequest};
use autonoetic_types::hooks::{HookAction, HookConfig, HookContext, HookEvent};
use tokio::sync::mpsc;

// ── helpers ────────────────────────────────────────────────────────────────

fn make_hook(
    event: HookEvent,
    action: HookAction,
    r#async: bool,
    params: serde_json::Value,
    allowed_agents: Vec<&str>,
) -> HookConfig {
    HookConfig {
        event,
        action,
        r#async,
        params: params
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect(),
        callback_allowlist: Vec::new(),
        allowed_agents: allowed_agents.into_iter().map(str::to_string).collect(),
    }
}

fn approval_resolved_ctx(request_id: &str, decision: &str) -> HookContext {
    let mut fields = HashMap::new();
    fields.insert("request_id".to_string(), request_id.to_string());
    fields.insert("decision".to_string(), decision.to_string());
    HookContext {
        event: HookEvent::ApprovalResolved,
        root_session_id: "root-test".to_string(),
        session_id: Some("child-test".to_string()),
        agent_id: Some("coder.default".to_string()),
        gateway_dir: None,
        fields,
    }
}

fn build_executor_with_channel(
    hooks: Vec<HookConfig>,
) -> (HookExecutor, mpsc::Receiver<HookSpawnRequest>) {
    let (tx, rx) = mpsc::channel::<HookSpawnRequest>(16);
    let mut exec = HookExecutor::new(
        hooks,
        None, // no gateway store needed for spawn-only tests
        4000,
        10,
    );
    exec.set_spawn_tx(tx);
    (exec, rx)
}

// ── tests ──────────────────────────────────────────────────────────────────

/// A correctly configured `agent.spawn` hook delivers a `HookSpawnRequest`
/// on the channel with the rendered message and expected agent_id.
#[tokio::test]
async fn test_agent_spawn_happy_path() {
    let hook = make_hook(
        HookEvent::ApprovalResolved,
        HookAction::AgentSpawn,
        true,
        serde_json::json!({
            "agent_id": "evaluator.default",
            "message_template": "Evaluate approval {{request_id}} — {{decision}}"
        }),
        vec![],
    );

    let (exec, mut rx) = build_executor_with_channel(vec![hook]);
    let ctx = approval_resolved_ctx("apr-123", "approved");

    exec.dispatch_async(ctx);

    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for HookSpawnRequest")
        .expect("channel closed unexpectedly");

    assert_eq!(req.agent_id, "evaluator.default");
    assert_eq!(
        req.message,
        "Evaluate approval apr-123 — approved",
        "template substitution should have applied"
    );
    assert!(
        req.session_id.starts_with("hook-spawn-"),
        "session_id should be prefixed hook-spawn-"
    );
    assert_eq!(req.root_session_id, "root-test");
}

/// When `allowed_agents` is set and the target is in the list, the spawn goes through.
#[tokio::test]
async fn test_agent_spawn_acl_allow() {
    let hook = make_hook(
        HookEvent::ApprovalResolved,
        HookAction::AgentSpawn,
        true,
        serde_json::json!({
            "agent_id": "evaluator.default",
            "message_template": "Evaluate {{request_id}}"
        }),
        vec!["evaluator.default"],
    );

    let (exec, mut rx) = build_executor_with_channel(vec![hook]);
    exec.dispatch_async(approval_resolved_ctx("apr-456", "approved"));

    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out")
        .expect("channel closed");

    assert_eq!(req.agent_id, "evaluator.default");
}

/// When `allowed_agents` is set and the target is NOT in the list, no spawn is sent.
#[tokio::test]
async fn test_agent_spawn_acl_block() {
    let hook = make_hook(
        HookEvent::ApprovalResolved,
        HookAction::AgentSpawn,
        true,
        serde_json::json!({
            "agent_id": "coder.default",
            "message_template": "Evaluate {{request_id}}"
        }),
        vec!["evaluator.default"],
    );

    let (exec, mut rx) = build_executor_with_channel(vec![hook]);
    exec.dispatch_async(approval_resolved_ctx("apr-789", "approved"));

    let result = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
    assert!(result.is_err(), "no HookSpawnRequest should be sent when ACL blocks");
}

/// When `params.agent_id` is absent, no spawn is sent.
#[tokio::test]
async fn test_agent_spawn_missing_agent_id() {
    let hook = make_hook(
        HookEvent::ApprovalResolved,
        HookAction::AgentSpawn,
        true,
        serde_json::json!({ "message_template": "Evaluate {{request_id}}" }),
        vec![],
    );

    let (exec, mut rx) = build_executor_with_channel(vec![hook]);
    exec.dispatch_async(approval_resolved_ctx("apr-000", "approved"));

    let result = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
    assert!(result.is_err(), "no HookSpawnRequest without agent_id");
}

/// `{{event}}` is substituted with the event's string name.
#[tokio::test]
async fn test_agent_spawn_event_placeholder() {
    let hook = make_hook(
        HookEvent::ApprovalResolved,
        HookAction::AgentSpawn,
        true,
        serde_json::json!({
            "agent_id": "evaluator.default",
            "message_template": "Event={{event}} req={{request_id}}"
        }),
        vec![],
    );

    let (exec, mut rx) = build_executor_with_channel(vec![hook]);
    exec.dispatch_async(approval_resolved_ctx("apr-evt", "rejected"));

    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await.expect("timed out").expect("closed");

    assert_eq!(req.message, "Event=approval.resolved req=apr-evt");
}

/// `agent.spawn` hooks with `async: false` are skipped — both via dispatch() and dispatch_async().
#[tokio::test]
async fn test_agent_spawn_sync_not_supported() {
    let hook = make_hook(
        HookEvent::ApprovalResolved,
        HookAction::AgentSpawn,
        false, // sync — should be rejected by agent_spawn()
        serde_json::json!({
            "agent_id": "evaluator.default",
            "message_template": "Evaluate {{request_id}}"
        }),
        vec![],
    );

    let (exec, mut rx) = build_executor_with_channel(vec![hook.clone()]);
    // Both dispatch paths should route through agent_spawn() which checks async flag.
    exec.dispatch(&approval_resolved_ctx("apr-sync1", "approved"));
    exec.dispatch_async(approval_resolved_ctx("apr-sync2", "approved"));

    let result = tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
    assert!(result.is_err(), "sync agent.spawn should produce no HookSpawnRequest");
}

/// When spawn channel is not wired, dispatching should not panic.
#[tokio::test]
async fn test_agent_spawn_no_channel_wired() {
    let hook = make_hook(
        HookEvent::ApprovalResolved,
        HookAction::AgentSpawn,
        true,
        serde_json::json!({
            "agent_id": "evaluator.default",
            "message_template": "Evaluate {{request_id}}"
        }),
        vec![],
    );

    // Do NOT call set_spawn_tx — spawn_tx remains None.
    let exec = HookExecutor::new(vec![hook], None, 4000, 10);
    exec.dispatch_async(approval_resolved_ctx("apr-nowire", "approved"));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // No assertion needed beyond "did not panic".
}

/// HookEvent serde uses dotted names — verify round-trip serialization matches config format.
#[test]
fn test_hook_event_serde_dotted_names() {
    let event: HookEvent = serde_json::from_str("\"approval.resolved\"").unwrap();
    assert_eq!(event, HookEvent::ApprovalResolved);

    let serialized = serde_json::to_string(&HookEvent::SessionClosed).unwrap();
    assert_eq!(serialized, "\"session.closed\"");

    let wj: HookEvent = serde_json::from_str("\"workflow.join.satisfied\"").unwrap();
    assert_eq!(wj, HookEvent::WorkflowJoinSatisfied);

    let pd: HookEvent = serde_json::from_str("\"policy.decision\"").unwrap();
    assert_eq!(pd, HookEvent::PolicyDecision);
    assert_eq!(serde_json::to_string(&HookEvent::PolicyDecision).unwrap(), "\"policy.decision\"");
}
