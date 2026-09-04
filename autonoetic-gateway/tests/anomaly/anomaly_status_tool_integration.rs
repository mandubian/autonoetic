//! `anomaly_status` — capability-free reporter-self read path (Ri-0.18
//! symmetry). Complements `anomaly_flag_integration.rs`.
//!
//! Verifies:
//!   - the tool is available on a manifest holding ZERO capabilities
//!   - the reporter can re-read its own filing, full record incl. observation
//!   - the list form returns only the caller's own filings, without
//!     observation text
//!   - another agent's flag reads as `not_found` — indistinguishable from an
//!     unknown id (no ledger-existence oracle)
//!   - an `AnomalyAdjudicate` holder can read any flag by id (informed
//!     adjudication, O-7) but the list form stays reporter-self
//!   - `resolve` on an `aflag-…` ref redirects to `anomaly_status`

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn manifest_for(agent_id: &str, caps: Vec<Capability>) -> AgentManifest {
    let mut m = TestManifest::new().agent_id(agent_id).build();
    m.capabilities = caps;
    m
}

fn zero_capability_manifest(agent_id: &str) -> AgentManifest {
    manifest_for(agent_id, vec![])
}

fn ombudsman_manifest() -> AgentManifest {
    manifest_for(
        "ombudsman.default",
        vec![Capability::AnomalyAdjudicate {
            patterns: vec!["*".to_string()],
        }],
    )
}

struct Harness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
}

fn make_harness() -> Harness {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("witness.default");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).expect("store opens"));
    Harness {
        _temp: temp,
        store,
        agent_dir,
        gateway_dir,
    }
}

fn invoke(
    h: &Harness,
    tool: &str,
    manifest: &AgentManifest,
    args_json: &str,
) -> serde_json::Value {
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();
    let raw = registry
        .execute(
            tool,
            manifest,
            &policy,
            &h.agent_dir,
            Some(&h.gateway_dir),
            args_json,
            Some("test-session"),
            Some("turn-000001"),
            Some(&gateway_config),
            Some(h.store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

fn file_flag(h: &Harness, manifest: &AgentManifest, subject: &str, observation: &str) -> String {
    let resp = invoke(
        h,
        "anomaly_flag",
        manifest,
        &format!(
            r#"{{"subject_ref": "{subject}", "observation": "{observation}", "severity": "high"}}"#
        ),
    );
    assert_eq!(resp["ok"], true, "filing must succeed: {resp}");
    resp["flag_id"].as_str().expect("flag_id present").to_string()
}

#[test]
fn tool_available_with_zero_capabilities() {
    let registry = default_registry();
    let manifest = zero_capability_manifest("witness.default");
    let defs = registry.available_definitions(&manifest);
    assert!(
        defs.iter().any(|d| d.name == "anomaly_status"),
        "anomaly_status must be available to a manifest holding zero capabilities \
         (Ri-0.18 symmetry: filing is capability-free, so is the self-read)"
    );
}

#[test]
fn reporter_reads_own_flag_with_full_content() {
    let h = make_harness();
    let manifest = zero_capability_manifest("witness.default");
    let flag_id = file_flag(&h, &manifest, "sess-suspect-1", "gate said pending but attestation said zero");

    let resp = invoke(
        &h,
        "anomaly_status",
        &manifest,
        &format!(r#"{{"flag_id": "{flag_id}"}}"#),
    );

    assert_eq!(resp["ok"], true, "{resp}");
    assert_eq!(resp["viewer"], "reporter");
    assert_eq!(resp["flag"]["flag_id"], flag_id);
    assert_eq!(resp["flag"]["status"], "pending");
    assert_eq!(resp["flag"]["severity"], "high");
    assert_eq!(resp["flag"]["subject_ref"], "sess-suspect-1");
    assert_eq!(resp["flag"]["reporter_agent_id"], "witness.default");
    assert_eq!(
        resp["flag"]["reporter_session_id"], "test-session",
        "provenance: the filing session is visible"
    );
    assert_eq!(
        resp["flag"]["observation"],
        "gate said pending but attestation said zero",
        "the reporter can re-read the observation it authored"
    );
    assert!(resp["flag"]["decision"].is_null(), "no decision yet");
}

#[test]
fn list_form_returns_only_own_filings_without_observation() {
    let h = make_harness();
    let witness = zero_capability_manifest("witness.default");
    let other = zero_capability_manifest("bystander.default");
    file_flag(&h, &witness, "sess-a", "first observation");
    file_flag(&h, &witness, "sess-b", "second observation");
    file_flag(&h, &other, "sess-c", "someone else's observation");

    let resp = invoke(&h, "anomaly_status", &witness, r#"{}"#);

    assert_eq!(resp["ok"], true, "{resp}");
    assert_eq!(resp["kind"], "anomaly_flag_list");
    assert_eq!(resp["count"], 2, "only the caller's own filings are listed");
    for flag in resp["flags"].as_array().expect("flags array") {
        assert_eq!(flag["subject_ref"].as_str().unwrap_or_default().len() > 0, true);
        assert!(
            flag.get("observation").is_none(),
            "the list form must not carry observation text"
        );
    }
}

#[test]
fn other_agents_flag_reads_as_not_found() {
    let h = make_harness();
    let filer = zero_capability_manifest("bystander.default");
    let prober = zero_capability_manifest("witness.default");
    let flag_id = file_flag(&h, &filer, "sess-x", "not the prober's flag");

    let resp = invoke(
        &h,
        "anomaly_status",
        &prober,
        &format!(r#"{{"flag_id": "{flag_id}"}}"#),
    );

    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "anomaly_flag_not_found");
    assert_eq!(resp["failure_class"], "bad_reference", "{resp}");
    assert_eq!(resp["retryable"], false);

    // Indistinguishable from an unknown id — no ledger-existence oracle.
    // (The message legitimately echoes the probed id; the shape — error
    // code, type, failure class — must be identical.)
    let unknown = invoke(
        &h,
        "anomaly_status",
        &prober,
        r#"{"flag_id": "aflag-does-not-exist"}"#,
    );
    assert_eq!(unknown["error"], resp["error"]);
    assert_eq!(unknown["error_type"], resp["error_type"]);
    assert_eq!(unknown["failure_class"], resp["failure_class"]);
    assert_eq!(unknown["repair_hint"], resp["repair_hint"]);
}

#[test]
fn adjudicator_reads_any_flag_by_id_but_list_stays_self() {
    let h = make_harness();
    let filer = zero_capability_manifest("witness.default");
    let flag_id = file_flag(&h, &filer, "sess-x", "the observation under review");
    let office = ombudsman_manifest();

    let resp = invoke(
        &h,
        "anomaly_status",
        &office,
        &format!(r#"{{"flag_id": "{flag_id}"}}"#),
    );
    assert_eq!(resp["ok"], true, "{resp}");
    assert_eq!(resp["viewer"], "adjudicator");
    assert_eq!(
        resp["flag"]["observation"],
        "the observation under review",
        "the decider must adjudicate informed, not blind (O-7)"
    );

    let list = invoke(&h, "anomaly_status", &office, r#"{}"#);
    assert_eq!(
        list["count"], 0,
        "the list form stays reporter-self: no queue discovery here"
    );
}

#[test]
fn status_filter_and_invalid_status() {
    let h = make_harness();
    let manifest = zero_capability_manifest("witness.default");
    let flag_id = file_flag(&h, &manifest, "sess-x", "odd");

    let pending = invoke(
        &h,
        "anomaly_status",
        &manifest,
        r#"{"status": "pending"}"#,
    );
    assert_eq!(pending["count"], 1);
    assert_eq!(pending["flags"][0]["flag_id"], flag_id);

    let dismissed = invoke(
        &h,
        "anomaly_status",
        &manifest,
        r#"{"status": "dismissed"}"#,
    );
    assert_eq!(dismissed["count"], 0);

    let invalid = invoke(
        &h,
        "anomaly_status",
        &manifest,
        r#"{"status": "catastrophic"}"#,
    );
    assert_eq!(invalid["ok"], false);
    assert_eq!(invalid["error_type"], "validation");
}

#[test]
fn resolve_redirects_aflag_refs_to_anomaly_status() {
    let h = make_harness();
    // resolve requires ReadAccess — the realistic caller (a planner that
    // can read) probing an aflag- ref must be pointed at the read tool.
    let manifest = manifest_for(
        "witness.default",
        vec![Capability::ReadAccess {
            scopes: vec!["session".to_string()],
        }],
    );
    let flag_id = file_flag(&h, &zero_capability_manifest("bystander.default"), "sess-x", "odd");

    let resp = invoke(
        &h,
        "resolve",
        &manifest,
        &format!(r#"{{"ref": "{flag_id}", "include": "content"}}"#),
    );

    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"], "use_anomaly_status");
    assert_eq!(resp["failure_class"], "bad_reference", "{resp}");
    assert!(
        resp["repair_hint"].as_str().unwrap_or_default().contains("anomaly_status"),
        "the redirect must point at the read tool: {resp}"
    );
}

#[test]
fn filing_response_advertises_the_read_path() {
    let h = make_harness();
    let manifest = zero_capability_manifest("witness.default");
    let resp = invoke(
        &h,
        "anomaly_flag",
        &manifest,
        r#"{"subject_ref": "sess-x", "observation": "odd"}"#,
    );
    assert_eq!(resp["ok"], true);
    assert!(
        resp["read_path"].as_str().unwrap_or_default().contains("anomaly_status"),
        "the filer must learn the read path at filing time: {resp}"
    );
}
