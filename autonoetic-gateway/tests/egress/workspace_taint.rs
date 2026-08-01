//! Agent workspace egress labels (RFC §11, #1001): content-movement laundering.
//!
//! Source rules are a firewall over *named* paths and cannot survive content
//! movement (`unzip ~/mail/mail.zip -d /tmp/w`, then reading the copy). The
//! workspace is the labeled unit instead: an exec that resolves restricted
//! narrows its agent's durable workspace label, any exec in the workspace
//! intersects that label — so the laundered copy stays labeled even in a fresh
//! session — and the workspace hop shows up in `egress.lineage` as its own
//! origin.

use crate::rpc_env::{env, rpc_as};
use autonoetic_gateway::router::JsonRpcResponse;
use autonoetic_gateway::runtime::egress_labeler::{
    workspace_taint_from_store, EgressLabeler, LabelRequest,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressConfig, EgressLabel, EgressRule};
use std::collections::HashMap;

async fn rpc(method: &str, params: serde_json::Value) -> JsonRpcResponse {
    rpc_as("workspace-taint-test", method, params).await
}

fn labeler_with_mail_rule() -> EgressLabeler {
    EgressLabeler::from_config(&EgressConfig {
        rules: vec![EgressRule {
            source: "sandbox.exec".to_string(),
            path: Some("~/mail/**".to_string()),
            label: EgressLabel::local_only(),
        }],
        ..Default::default()
    })
}

/// The #988 walk-through, end to end: content moved into the workspace stays
/// labeled across sessions, even with no rules in the new session.
#[tokio::test]
async fn content_moved_into_the_workspace_stays_labeled_across_sessions() {
    let e = env();
    let agent = "coder.abc";

    // Session 1: an exec reads a rule-labeled path (and moves the content into
    // the workspace, which static analysis cannot follow). The exec resolves
    // local_only and narrows the agent's durable workspace label.
    let out1 = labeler_with_mail_rule()
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"unzip ~/mail/mail.zip -d /tmp/w && cat /tmp/w/inbox.mbox"}"#,
                tool_call_id: "tc_ws_1",
                artifact_id: None,
            },
            None,
            "ws-root/coder.abc-1",
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        )
        .expect("rule-matched exec must be restricted");
    assert_eq!(out1.label, EgressLabel::local_only());
    assert_eq!(
        e.store.get_workspace_egress_label(agent).unwrap(),
        Some(EgressLabel::local_only()),
        "the workspace narrowed with the exec"
    );

    // Session 2 — a fresh session with NO rules: the laundered copy is still
    // labeled, because the exec runs in a workspace that carries the label.
    let out2 = EgressLabeler::from_config(&EgressConfig::default())
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"cat /tmp/w/inbox.mbox"}"#,
                tool_call_id: "tc_ws_2",
                artifact_id: None,
            },
            None,
            "ws-root/coder.abc-2",
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        )
        .expect("workspace label must carry the taint into the new session");
    assert_eq!(out2.label, EgressLabel::local_only());
}

/// The workspace hop is its own `egress.lineage` origin: "why is this
/// labeled?" answers "because this agent's workspace is", not a rule that
/// cannot match the moved path.
#[tokio::test]
async fn workspace_hop_is_its_own_lineage_origin() {
    let e = env();
    let agent = "coder.lineage";

    // One restricted exec narrows the workspace…
    let out1 = labeler_with_mail_rule()
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"cp ~/mail/inbox/1 /tmp/w/"}"#,
                tool_call_id: "tc_ws_l1",
                artifact_id: None,
            },
            None,
            "ws-root/coder.lineage-1",
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        )
        .expect("restricted");
    assert_eq!(out1.label, EgressLabel::local_only());

    // …then a clean exec (no rule fires) in a *sibling* session inherits it.
    let out2 = EgressLabeler::from_config(&EgressConfig::default())
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"cat /tmp/w/inbox.mbox"}"#,
                tool_call_id: "tc_ws_l2",
                artifact_id: None,
            },
            None,
            "ws-root/coder.lineage-2",
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        )
        .expect("workspace label applies");
    assert_eq!(out2.label, EgressLabel::local_only());

    let resp = rpc(
        "egress.lineage",
        serde_json::json!({
            "root_session_id": "ws-root",
            "from": out2.envelope_id,
        }),
    )
    .await;
    assert!(resp.error.is_none(), "lineage error: {:?}", resp.error);
    let result = resp.result.unwrap();
    let nodes = result["nodes"].as_array().unwrap();
    assert!(!nodes.is_empty());
    let node = &nodes[0];
    assert_eq!(
        node["origin"],
        serde_json::json!("workspace_label"),
        "the hop must report the workspace as its origin: {node}"
    );
    assert_eq!(
        node["workspace_agents"],
        serde_json::json!([agent]),
        "the node must name the workspace: {node}"
    );
}

/// Structured tools do not run in a workspace: they neither inherit its label
/// nor narrow it — source rules remain the mechanism for them.
#[tokio::test]
async fn structured_tools_never_touch_workspace_labels() {
    let e = env();
    let agent = "coder.struct";

    // A labeled workspace does not bleed into structured reads.
    e.store
        .restrict_workspace_egress_label(agent, &EgressLabel::no_remote_model())
        .unwrap();
    let out = EgressLabeler::from_config(&EgressConfig::default())
        .label_tool_result(
            &LabelRequest {
                tool: "content.read",
                arguments_json: r#"{"path":"/tmp/w/inbox.mbox"}"#,
                tool_call_id: "tc_ws_3",
                artifact_id: None,
            },
            None,
            "ws-root/coder.struct-1",
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        );
    assert!(out.is_none(), "structured reads must not inherit workspace taint");

    // And a restricted structured result does not narrow the workspace.
    let l = EgressLabeler::from_config(&EgressConfig {
        rules: vec![EgressRule {
            source: "content.read".to_string(),
            path: Some("~/mail/**".to_string()),
            label: EgressLabel::local_only(),
        }],
        ..Default::default()
    });
    let req = LabelRequest {
        tool: "content.read",
        arguments_json: r#"{"path":"~/mail/inbox/1"}"#,
        tool_call_id: "tc_ws_4",
        artifact_id: None,
    };
    let out = l
        .label_tool_result(
            &req,
            None,
            "ws-root/coder.struct-1",
            agent,
            Some("turn-1"),
            Some(&e.store),
            &HashMap::new(),
        )
        .expect("rule-matched structured read restricted");
    assert_eq!(out.label, EgressLabel::local_only());
    assert_eq!(
        e.store.get_workspace_egress_label(agent).unwrap(),
        Some(EgressLabel::no_remote_model()),
        "structured tools never write the workspace label"
    );
}

/// A store read failure must not become a durable workspace restriction: the
/// result fails closed to `local_only` and names the agent, the read is
/// reported as failed (so the caller knows), and nothing is written back —
/// only an exec that *actually* resolved restricted may tighten the durable
/// label. A transient read error is a per-result decision, not a ratchet.
#[tokio::test]
async fn store_read_failure_fails_closed_and_skips_the_write_back() {
    let tmp = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(GatewayStore::open(tmp.path()).unwrap());
    // Force a read error by dropping the table out from under the store.
    let raw = rusqlite::Connection::open(tmp.path().join("gateway.db")).unwrap();
    raw.execute_batch("DROP TABLE agent_workspace_egress_labels")
        .unwrap();

    // The exec still fails closed…
    let out = EgressLabeler::from_config(&EgressConfig::default())
        .label_tool_result(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"cat /tmp/w/inbox.mbox"}"#,
                tool_call_id: "tc_ws_fail",
                artifact_id: None,
            },
            None,
            "ws-root/coder.fail-1",
            "coder.fail",
            Some("turn-1"),
            Some(&store),
            &HashMap::new(),
        )
        .expect("fail-closed result");
    assert_eq!(out.label, EgressLabel::local_only());

    // …the read side reports the failure (no silent persistence),…
    let (_label, applied, failed) = workspace_taint_from_store("coder.fail", store.as_ref());
    assert_eq!(applied, vec!["coder.fail".to_string()]);
    assert!(failed, "the read must be reported as failed");
    // …and nothing was written back: with the table restored, the workspace
    // still carries no label.
    raw.execute_batch(
        "CREATE TABLE agent_workspace_egress_labels (
            agent_id TEXT PRIMARY KEY,
            label_json TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    )
    .unwrap();
    assert_eq!(
        store.get_workspace_egress_label("coder.fail").unwrap(),
        None,
        "a failed read must not ratchet the durable workspace label"
    );
    // A healthy read marks no failure.
    let (_label, applied, failed) = workspace_taint_from_store("coder.fail", store.as_ref());
    assert!(applied.is_empty());
    assert!(!failed);
}
