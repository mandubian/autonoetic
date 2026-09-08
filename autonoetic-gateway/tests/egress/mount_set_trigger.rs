//! Exec mount-set trigger (RFC sandbox-mount-allow-set §8, #1296 item 3).
//!
//! The exec path scan has two halves: the *advisory* command/script scan
//! (RFC §4.2) and — since #1002 — the *mechanical* mount-set feed. The
//! gateway itself asserts the exec's mount set (`compose_mount_set`, lifted
//! from the tool result into the durable trace), so a labeled path **in the
//! mount set** fires its rule whether or not any command text references it.
//! These tests pin the contract end to end against a real store: label
//! resolution, the `egress.envelope_labeled` audit event (which must
//! distinguish mechanical triggers from the heuristic), and the workspace
//! ratchet.

use crate::rpc_env::env;
use autonoetic_gateway::runtime::egress_labeler::{
    mount_set_in_result, EgressLabeler, LabelRequest, PriorLabeledResult,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::egress::{EgressConfig, EgressLabel, EgressRule};
use std::collections::HashMap;
use std::sync::Arc;

fn labeler_with(rules: Vec<EgressRule>) -> EgressLabeler {
    EgressLabeler::from_config(&EgressConfig {
        rules,
        ..Default::default()
    })
}

fn mail_rule() -> EgressRule {
    EgressRule {
        source: "sandbox.exec".to_string(),
        path: Some("/home/alice/mail/**".to_string()),
        label: EgressLabel::local_only(),
    }
}

fn no_prior() -> HashMap<String, PriorLabeledResult> {
    HashMap::new()
}

fn envelope_events(store: &Arc<GatewayStore>, session_id: &str) -> Vec<serde_json::Value> {
    store
        .search_causal_events(Some(session_id), None, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.action == "egress.envelope_labeled")
        .filter_map(|e| e.payload.and_then(|p| serde_json::from_str(&p).ok()))
        .collect()
}

/// The mechanical trigger, end to end: the command never references the
/// labeled path, but the gateway-asserted mount set exposes its root — the
/// result is labeled, the event names the trigger as mechanical, and the
/// workspace ratchet tightens.
#[tokio::test]
async fn mount_overlap_labels_and_audits_the_exec() {
    let e = env();
    let agent = "coder.mount";
    let session = "mount-root/coder.mount-1";

    let mount_entries = vec![
        "rw:/tmp/w".to_string(),
        "ro:/home/alice/mail".to_string(),
    ];
    let out = labeler_with(vec![mail_rule()])
        .label_tool_result_with_mounts(
            &LabelRequest {
                tool: "sandbox_exec",
                arguments_json: r#"{"command":"python3 /tmp/w/parse.py"}"#,
                tool_call_id: "tc_mount_e2e_1",
                artifact_id: None,
            },
            None,
            session,
            agent,
            Some("turn-1"),
            Some(&e.store),
            &no_prior(),
            Some(&mount_entries),
        )
        .expect("a labeled path in the mount set must restrict");
    assert_eq!(out.label, EgressLabel::local_only());

    let events = envelope_events(&e.store, session);
    assert_eq!(events.len(), 1, "one envelope_labeled event: {events:?}");
    let payload = &events[0];
    assert_eq!(
        payload["mount_triggers_applied"],
        serde_json::json!(["/home/alice/mail/**"]),
        "the event must record which pattern fired mechanically"
    );
    assert_eq!(
        payload["matched_paths"],
        serde_json::json!(["/home/alice/mail/**"]),
    );
    assert!(payload["matched_rules"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "sandbox.exec:/home/alice/mail/**"));

    // The mechanical trigger participates in the workspace ratchet.
    assert_eq!(
        e.store.get_workspace_egress_label(agent).unwrap(),
        Some(EgressLabel::local_only()),
    );
}

/// No mount overlap and no command reference → nothing labeled, nothing
/// emitted. The RFC §8 feed must not blanket-restrict execs that merely run
/// in a session whose config has path rules.
#[tokio::test]
async fn without_mount_overlap_nothing_is_labeled_or_emitted() {
    let e = env();
    let session = "mount-root/coder.mount-2";

    let out = labeler_with(vec![mail_rule()]).label_tool_result_with_mounts(
        &LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"python3 /tmp/w/parse.py"}"#,
            tool_call_id: "tc_mount_e2e_2",
            artifact_id: None,
        },
        None,
        session,
        "coder.mount",
        Some("turn-1"),
        Some(&e.store),
        &no_prior(),
        None,
    );
    assert!(out.is_none());
    assert!(
        envelope_events(&e.store, session).is_empty(),
        "unrestricted results emit nothing"
    );

    // Sentinel and marker entries name no host path — they must not turn the
    // legacy blanket bind into a blanket trigger either.
    let sentinel = vec!["ro:host_root".to_string(), "truncated:+2".to_string()];
    let out = labeler_with(vec![mail_rule()]).label_tool_result_with_mounts(
        &LabelRequest {
            tool: "sandbox_exec",
            arguments_json: r#"{"command":"echo hi"}"#,
            tool_call_id: "tc_mount_e2e_3",
            artifact_id: None,
        },
        None,
        session,
        "coder.mount",
        Some("turn-1"),
        Some(&e.store),
        &no_prior(),
        Some(&sentinel),
    );
    assert!(out.is_none(), "host_root sentinel never fires path rules");
}

/// Both halves can fire at once: the command references one labeled path
/// (advisory) while the mount set exposes another (mechanical). The labels
/// intersect and the event distinguishes the two inputs.
#[tokio::test]
async fn mechanical_and_advisory_triggers_intersect() {
    let e = env();
    let session = "mount-root/coder.mount-3";

    let rules = vec![
        mail_rule(),
        EgressRule {
            source: "sandbox.exec".to_string(),
            path: Some("/tmp/w/**".to_string()),
            label: EgressLabel::no_remote_model(),
        },
    ];
    let mount_entries = vec!["ro:/home/alice/mail".to_string()];
    let out = labeler_with(rules)
        .label_tool_result_with_mounts(
            &LabelRequest {
                tool: "sandbox_exec",
                // `/tmp/w` referenced in text (advisory); the mail root only
                // mounted (mechanical).
                arguments_json: r#"{"command":"python3 /tmp/w/parse.py"}"#,
                tool_call_id: "tc_mount_e2e_4",
                artifact_id: None,
            },
            None,
            session,
            "coder.mount",
            Some("turn-1"),
            Some(&e.store),
            &no_prior(),
            Some(&mount_entries),
        )
        .expect("both triggers restrict");
    assert_eq!(out.label, EgressLabel::local_only(), "intersection wins");

    let events = envelope_events(&e.store, session);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["mount_triggers_applied"],
        serde_json::json!(["/home/alice/mail/**"]),
        "only the mount-overlapping pattern is mechanical"
    );
    let matched = events[0]["matched_paths"].as_array().unwrap();
    assert!(matched.iter().any(|p| p == "/home/alice/mail/**"));
    assert!(matched.iter().any(|p| p == "/tmp/w/**"));
}

/// The lifter accepts exactly the shape the gateway composes: a JSON array of
/// strings under `mount_set` in the tool's own result. Non-JSON sandbox
/// output, missing keys, and malformed arrays yield nothing.
#[tokio::test]
async fn mount_set_lifter_accepts_only_gateway_shape() {
    assert_eq!(
        mount_set_in_result(
            r#"{"ok":true,"exit_code":0,"mount_set":["ro:host_root","rw:/tmp/a","ro:/home/alice/mail"]}"#
        ),
        Some(vec![
            "ro:host_root".to_string(),
            "rw:/tmp/a".to_string(),
            "ro:/home/alice/mail".to_string(),
        ]),
    );
    assert_eq!(mount_set_in_result("unzip: done"), None);
    assert_eq!(mount_set_in_result(r#"{"ok":true}"#), None);
    assert_eq!(mount_set_in_result(r#"{"mount_set":[]}"#), None);
    assert_eq!(mount_set_in_result(r#"{"mount_set":"ro:/etc"}"#), None);
}
