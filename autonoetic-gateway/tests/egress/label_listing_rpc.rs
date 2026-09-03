//! `labels.list` operator RPC (#974, RFC §9.3) — read-only, metadata-only,
//! root-tree scoped. Covers the six sections, child-session scoping, the three
//! label filters, the per-source store-error surfacing, and the metadata-only
//! invariant (no `content`/`stdout`/`message` keys leak into the response).

use crate::rpc_env::{env, rpc_as};
use autonoetic_gateway::router::JsonRpcResponse;
use autonoetic_gateway::scheduler::gateway_store::{AgentMessageRecord, GatewayStore};
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use autonoetic_types::causal_chain::{CausalEventRecord, ExecutionTraceRecord};
use autonoetic_types::egress::EgressLabel;
use autonoetic_types::memory::MemoryObject;

async fn rpc(method: &str, params: serde_json::Value) -> JsonRpcResponse {
    rpc_as("labels-test", method, params).await
}

async fn labels_list(params: serde_json::Value) -> serde_json::Value {
    let resp = rpc("labels.list", params).await;
    assert!(
        resp.error.is_none(),
        "labels.list returned error: {:?}",
        resp.error
    );
    resp.result.expect("labels.list result")
}

/// Seed an `egress.envelope_labeled` causal event mirroring the runtime
/// emitter's payload shape (egress_labeler::emit_envelope_labeled_event).
fn seed_envelope(
    store: &GatewayStore,
    session_id: &str,
    turn_id: Option<&str>,
    envelope_id: &str,
    tool: &str,
    label: &EgressLabel,
    matched_rules: Vec<&str>,
    matched_rule_scopes: Vec<&str>,
    parent_envelope_ids: Vec<&str>,
    artifact_labels: Vec<&str>,
    taint_applied: bool,
) {
    let payload = serde_json::json!({
        "envelope_id": envelope_id,
        "tool_call_id": "call_1",
        "tool_name": tool,
        "label": serde_json::to_value(label).unwrap(),
        "matched_rules": matched_rules,
        "matched_rule_scopes": matched_rule_scopes
            .into_iter()
            .map(|s| serde_json::json!({ "rule": format!("src:{s}"), "scope": s }))
            .collect::<Vec<_>>(),
        "parent_envelope_ids": parent_envelope_ids,
        "taint_applied": taint_applied,
        "artifact_labels_applied": artifact_labels,
        "bundle_floor_applied": false,
        "resolution": "operator_and_session_rule",
    });
    let event = CausalEventRecord {
        event_id: format!("env-{}", envelope_id),
        agent_id: "planner.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: turn_id.map(String::from),
        event_seq: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "egress".to_string(),
        action: "egress.envelope_labeled".to_string(),
        status: "active".to_string(),
        enforced_rules: vec!["I-6".to_string()],
        target: Some(envelope_id.to_string()),
        payload: Some(payload.to_string()),
        payload_ref: None,
        evidence_ref: None,
        reason: Some("egress_label_resolved".to_string()),
    };
    store.create_causal_event(&event).expect("seed event");
}

fn seed_labeled_memory(store: &GatewayStore, scope: &str, memory_id: &str) {
    let mut mem = MemoryObject::new(
        memory_id.to_string(),
        scope.to_string(),
        "lead".to_string(),
        "lead".to_string(),
        "r".to_string(),
        "SECRET BODY".to_string(),
    );
    mem.egress_label = Some(EgressLabel::local_only());
    store.memory_upsert(&mem).expect("memory upsert");
}

fn seed_labeled_artifact_ref(store: &GatewayStore, scope_id: &str, artifact_id: &str, ref_id: &str) {
    // Restrict the artifact's label first (creates the sidecar row), then mint
    // a session-scoped ref that ties it to the queried root tree.
    store
        .restrict_artifact_egress_label(artifact_id, &EgressLabel::local_only())
        .expect("restrict artifact label");
    let rec = ArtifactRefRecord {
        ref_id: ref_id.to_string(),
        scope_type: ArtifactRefScopeType::Session,
        scope_id: scope_id.to_string(),
        artifact_id: artifact_id.to_string(),
        artifact_manifest_digest: "manifest-d".to_string(),
        artifact_canonical_digest: "canonical-d".to_string(),
        created_by_agent_id: "a".to_string(),
        created_at: "2026-07-31T00:00:00Z".to_string(),
        expires_at: None,
        revoked_at: None,
    };
    store.create_artifact_ref(&rec).expect("artifact ref");
}

fn seed_labeled_trace(store: &GatewayStore, session_id: &str, trace_id: &str) {
    let trace = ExecutionTraceRecord {
        trace_id: trace_id.to_string(),
        event_id: None,
        agent_id: "a".to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        timestamp: "t".to_string(),
        tool_name: "sandbox_exec".to_string(),
        command: Some("SECRET CMD".to_string()),
        exit_code: Some(0),
        stdout: Some("SECRET STDOUT".to_string()),
        stderr: Some("SECRET ERR".to_string()),
        duration_ms: 1,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
        egress_label: Some(EgressLabel::local_only()),
        mount_set: None,
    };
    store.create_execution_trace(&trace).expect("exec trace");
}

fn seed_labeled_message(store: &GatewayStore, session_id: &str, message_id: &str) {
    let rec = AgentMessageRecord {
        message_id: message_id.to_string(),
        sender_session_id: session_id.to_string(),
        sender_agent_id: "lead".to_string(),
        target_pattern: "*".to_string(),
        message: "SECRET MESSAGE BODY".to_string(),
        created_at: "t".to_string(),
        egress_label: Some(EgressLabel::local_only()),
    };
    store.save_agent_message(&rec).expect("save message");
}

#[tokio::test]
async fn labels_list_returns_all_sections_root_scoped() {
    let env = env();
    let root = "ll-root";
    let child = "ll-root/coder.abc";

    // Envelope in a child session.
    seed_envelope(
        &env.store,
        child,
        Some("turn-7"),
        "env_child1",
        "email.read",
        &EgressLabel::local_only(),
        vec!["src:email"],
        vec!["session"],
        vec![],
        vec![],
        true,
    );
    // Envelope in an unrelated session — must NOT appear.
    seed_envelope(
        &env.store,
        "other-root/x",
        Some("turn-1"),
        "env_other",
        "fs.read",
        &EgressLabel::local_only(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );

    env.store
        .set_session_egress_taint(root, &EgressLabel::local_only())
        .unwrap();
    env.store
        .set_session_egress_taint(child, &EgressLabel::no_remote_model())
        .unwrap();
    // `/private` pin state (#977): provider constraint surfaces alongside the
    // taint sections so a freshly pinned room shows the pin even before any
    // content has been labeled.
    env.store
        .set_egress_session_policy(
            root,
            &autonoetic_types::egress::EgressSessionPolicy {
                rules: vec![],
                default_label: None,
                provider_constraint: Some(
                    autonoetic_types::egress::ProviderConstraint::LocalOnly,
                ),
            },
            "operator",
        )
        .unwrap();

    seed_labeled_memory(&env.store, "session:ll-root", "mem-1");
    seed_labeled_memory(&env.store, "session:other-root", "mem-other");
    seed_labeled_artifact_ref(&env.store, child, "art_aaaa1111aaaa", "ar.aaa");

    let resp = labels_list(serde_json::json!({ "root_session_id": root })).await;

    // ---- current taint ----
    assert_eq!(
        resp["current_taint"],
        serde_json::to_value(EgressLabel::local_only()).unwrap()
    );

    // ---- provider constraint (pinned via /private) ----
    assert_eq!(resp["provider_constraint"], serde_json::json!("local_only"));

    // ---- envelopes: only the child's, with provenance fields round-tripped ----
    let envelopes = resp["envelopes"].as_array().unwrap();
    assert_eq!(envelopes.len(), 1, "only root-tree envelopes");
    let env_row = &envelopes[0];
    assert_eq!(env_row["envelope_id"], "env_child1");
    assert_eq!(env_row["session_id"], child);
    assert_eq!(env_row["turn_id"], "turn-7");
    assert_eq!(env_row["tool_name"], "email.read");
    assert_eq!(env_row["resolution"], "operator_and_session_rule");
    assert_eq!(env_row["taint_applied"], true);
    assert_eq!(
        env_row["matched_rules"],
        serde_json::json!(["src:email"])
    );
    assert_eq!(
        env_row["matched_rule_scopes"],
        serde_json::json!(["session"])
    );

    // ---- session taints: root + child, not other-root ----
    let taints = resp["session_taints"].as_array().unwrap();
    let taint_sessions: Vec<String> = taints
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(taint_sessions.iter().any(|s| s == root));
    assert!(taint_sessions.iter().any(|s| s == child));
    assert!(!taint_sessions.iter().any(|s| s == "other-root/x"));

    // ---- memories: only the root-scoped one ----
    let mems = resp["memories"].as_array().unwrap();
    assert_eq!(mems.len(), 1);
    assert_eq!(mems[0]["memory_id"], "mem-1");

    // ---- artifacts: only the child's ref ----
    let arts = resp["artifacts"].as_array().unwrap();
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0]["artifact_id"], "art_aaaa1111aaaa");
    assert_eq!(arts[0]["ref_id"], "ar.aaa");
}

#[tokio::test]
async fn labels_list_filter_cannot_reach_remote_model() {
    let env = env();
    let root = "flt-root";
    seed_envelope(
        &env.store,
        root,
        Some("t1"),
        "env_lo",
        "email.read",
        &EgressLabel::local_only(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );
    seed_envelope(
        &env.store,
        root,
        Some("t2"),
        "env_nrm",
        "doc.read",
        &EgressLabel::no_remote_model(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );
    seed_envelope(
        &env.store,
        root,
        Some("t3"),
        "env_unr",
        "doc.read",
        &EgressLabel::unrestricted(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );

    let resp = labels_list(serde_json::json!({
        "root_session_id": root,
        "cannot_reach": "remote_model",
    }))
    .await;
    let ids: Vec<_> = resp["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["envelope_id"].as_str().unwrap())
        .collect();
    // local_only + no_remote_model withheld from remote_model; unrestricted dropped.
    assert!(ids.contains(&"env_lo"));
    assert!(ids.contains(&"env_nrm"));
    assert!(!ids.contains(&"env_unr"));
}

#[tokio::test]
async fn labels_list_filter_named_label() {
    let env = env();
    let root = "nl-root";
    seed_envelope(
        &env.store,
        root,
        None,
        "env_a",
        "t",
        &EgressLabel::local_only(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );
    seed_envelope(
        &env.store,
        root,
        None,
        "env_b",
        "t",
        &EgressLabel::no_remote_model(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );

    let resp = labels_list(serde_json::json!({
        "root_session_id": root,
        "named_label": "local_only",
    }))
    .await;
    let ids: Vec<_> = resp["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["envelope_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["env_a"]);
}

#[tokio::test]
async fn labels_list_filter_by_turn() {
    let env = env();
    let root = "tu-root";
    seed_envelope(
        &env.store,
        root,
        Some("turn-1"),
        "env_tu1",
        "t",
        &EgressLabel::local_only(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );
    seed_envelope(
        &env.store,
        root,
        Some("turn-2"),
        "env_tu2",
        "t",
        &EgressLabel::local_only(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );

    let resp = labels_list(serde_json::json!({
        "root_session_id": root,
        "turn_id": "turn-2",
    }))
    .await;
    let ids: Vec<_> = resp["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["envelope_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["env_tu2"]);
}

#[tokio::test]
async fn labels_list_invalid_params_returns_32602() {
    let resp = rpc("labels.list", serde_json::json!({})).await;
    let err = resp.error.expect("expected -32602");
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("labels.list"));
}

#[tokio::test]
async fn labels_list_metadata_only_no_content_fields() {
    let env = env();
    let root = "md-root";
    seed_envelope(&env.store, root, None, "env_md", "t", &EgressLabel::local_only(), vec![], vec![], vec![], vec![], false);
    seed_labeled_memory(&env.store, "session:md-root", "mem-md");
    seed_labeled_trace(&env.store, root, "tr-md");
    seed_labeled_message(&env.store, root, "msg-md");

    let resp = labels_list(serde_json::json!({ "root_session_id": root })).await;
    let serialized = serde_json::to_string(&resp).unwrap();

    // The defining invariant: no content of any kind is ever returned.
    for forbidden in [
        "SECRET BODY",
        "SECRET STDOUT",
        "SECRET ERR",
        "SECRET CMD",
        "SECRET MESSAGE BODY",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "labels.list leaked content: {forbidden}"
        );
    }
    // And structurally: memory rows expose no `content` key, traces expose no
    // stdout/stderr/command keys, messages expose no message key.
    assert!(resp["memories"][0].as_object().unwrap().get("content").is_none());
    let trace = &resp["execution_traces"][0];
    for k in ["stdout", "stderr", "command", "arguments", "result"] {
        assert!(trace.get(k).is_none(), "trace leaked {k}");
    }
    assert!(resp["agent_messages"][0].as_object().unwrap().get("message").is_none());
}

#[tokio::test]
async fn labels_list_truncated_when_envelope_cap_hit() {
    let env = env();
    let root = "tr-root";
    // Seed more envelopes than a tiny limit.
    for i in 0..5 {
        seed_envelope(
            &env.store,
            root,
            None,
            &format!("env_tr_{i}"),
            "t",
            &EgressLabel::local_only(),
            vec![],
            vec![],
            vec![],
            vec![],
            false,
        );
    }
    let resp = labels_list(serde_json::json!({
        "root_session_id": root,
        "limit": 3,
    }))
    .await;
    assert_eq!(resp["truncated"], true);
    // Only up-to-limit envelopes are returned (the query caps at the limit).
    assert!(resp["envelopes"].as_array().unwrap().len() <= 3);
}

#[tokio::test]
async fn labels_list_can_reach_filter_includes_only_permitting() {
    let env = env();
    let root = "cr-root";
    // local_only permits MemoryPersist; no_remote_model does too; unrestricted does too.
    env.store
        .set_session_egress_taint(root, &EgressLabel::local_only())
        .unwrap();
    env.store
        .set_session_egress_taint(&format!("{root}/c"), &EgressLabel::no_remote_model())
        .unwrap();
    let resp = labels_list(serde_json::json!({
        "root_session_id": root,
        "can_reach": "memory_persist",
    }))
    .await;
    let sessions: Vec<String> = resp["session_taints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(sessions.iter().any(|s| s == root));
    // No session is excluded: both named labels allow memory_persist.
    assert_eq!(resp["session_taints"].as_array().unwrap().len(), 2);
    // Sanity: Sink::Network is allowed by no_remote_model but NOT local_only.
    let resp2 = labels_list(serde_json::json!({
        "root_session_id": root,
        "can_reach": "network",
    }))
    .await;
    assert_eq!(resp2["session_taints"].as_array().unwrap().len(), 1);
    assert_eq!(
        resp2["session_taints"][0]["session_id"],
        format!("{root}/c")
    );
}

#[tokio::test]
async fn labels_list_unrestricted_envelopes_are_listed() {
    // An unrestricted envelope still has a label row in the listing — the
    // operator sees "everything, including what's unrestricted", and applies a
    // filter to narrow. Confirms unrestricted isn't silently dropped.
    let env = env();
    let root = "unr-root";
    seed_envelope(
        &env.store,
        root,
        None,
        "env_unr_list",
        "doc.read",
        &EgressLabel::unrestricted(),
        vec![],
        vec![],
        vec![],
        vec![],
        false,
    );
    let resp = labels_list(serde_json::json!({ "root_session_id": root })).await;
    let ids: Vec<_> = resp["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["envelope_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"env_unr_list"));
    // No /private policy ⇒ provider_constraint is absent.
    assert!(resp.get("provider_constraint").is_none(), "{resp}");
    // And a cannot_reach filter drops it.
    let resp2 = labels_list(serde_json::json!({
        "root_session_id": root,
        "cannot_reach": "remote_model",
    }))
    .await;
    assert_eq!(resp2["envelopes"].as_array().unwrap().len(), 0);
}
