//! Anomaly flag — capability-free reporting (future Ri-0.18, issue #770 part
//! C.1). Mirrors `constitution_rights_amendment_proposal.rs`'s harness shape.
//!
//! Verifies:
//!   - the tool is available on a manifest holding ZERO capabilities
//!   - executing the tool inserts a durable row with an `aflag-` id, status
//!     `pending`, and correct reporter attribution
//!   - a causal event `anomaly_flag.filed` with `enforced_rules: ["Ri-0.18"]`
//!     exists
//!   - the tool response carries `ok: true` and `flag_id`
//!   - the per-reporter spam triage bound (#770): filings beyond
//!     `max_pending_anomaly_flags_per_reporter` are rejected loudly with
//!     `anomaly_flag_flood`

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use std::sync::Arc;
use tempfile::tempdir;

fn zero_capability_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: AgentIdentity {
            id: "witness.default".to_string(),
            name: "witness".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![],
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        script_input_mode: Default::default(),
        gateway_url: None,
        gateway_token: None,
        allowed_tool_tiers: vec![],
        excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

struct Harness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
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
    }
}

fn invoke(h: &Harness, manifest: &AgentManifest, args_json: &str) -> serde_json::Value {
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();
    let raw = registry
        .execute(
            "anomaly_flag",
            manifest,
            &policy,
            &h.agent_dir,
            None,
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

#[test]
fn tool_available_with_zero_capabilities() {
    let registry = default_registry();
    let manifest = zero_capability_manifest();
    let defs = registry.available_definitions(&manifest);
    assert!(
        defs.iter().any(|d| d.name == "anomaly_flag"),
        "anomaly_flag must be available to a manifest holding zero capabilities"
    );
}

#[test]
fn filing_persists_durable_row_with_reporter_attribution() {
    let h = make_harness();
    let manifest = zero_capability_manifest();

    let resp = invoke(
        &h,
        &manifest,
        r#"{
            "subject_ref": "sess-suspect-1",
            "observation": "tool call bypassed an expected approval gate",
            "evidence_refs": ["evt-aaaa"],
            "severity": "high"
        }"#,
    );

    assert_eq!(resp["ok"], true);
    let flag_id = resp["flag_id"].as_str().expect("flag_id present");
    assert!(flag_id.starts_with("aflag-"), "durable id format");
    assert_eq!(resp["status"], "pending");
    assert_eq!(resp["severity"], "high");
    assert!(resp["message"].as_str().unwrap_or_default().contains("cannot be silently dropped"));

    let row = h
        .store
        .get_anomaly_flag(flag_id)
        .expect("query")
        .expect("row exists");
    assert_eq!(row.status, "pending");
    assert_eq!(row.subject_ref, "sess-suspect-1");
    assert_eq!(row.reporter_agent_id, "witness.default");
    assert_eq!(row.reporter_session_id.as_deref(), Some("test-session"));
    assert_eq!(row.severity, "high");
    assert!(matches!(row.evidence_json, serde_json::Value::Array(ref a) if a.len() == 1));
}

#[test]
fn filing_emits_causal_event_tagged_ri_0_18() {
    let h = make_harness();
    let manifest = zero_capability_manifest();

    let resp = invoke(
        &h,
        &manifest,
        r#"{"subject_ref": "sess-suspect-2", "observation": "unexpected network call"}"#,
    );
    assert_eq!(resp["ok"], true);

    let events = h
        .store
        .search_causal_events(Some("test-session"), None, 100)
        .expect("query causal events");
    let flagged = events
        .iter()
        .find(|e| e.category == "anomaly_flag" && e.action == "filed")
        .expect("anomaly_flag.filed causal event exists");
    assert_eq!(flagged.enforced_rules, vec!["Ri-0.18".to_string()]);
    assert_eq!(flagged.target.as_deref(), Some("sess-suspect-2"));
}

#[test]
fn empty_subject_ref_rejected() {
    let h = make_harness();
    let manifest = zero_capability_manifest();
    let resp = invoke(
        &h,
        &manifest,
        r#"{"subject_ref": "   ", "observation": "something odd"}"#,
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
}

#[test]
fn empty_observation_rejected() {
    let h = make_harness();
    let manifest = zero_capability_manifest();
    let resp = invoke(
        &h,
        &manifest,
        r#"{"subject_ref": "sess-x", "observation": ""}"#,
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
}

#[test]
fn unknown_severity_rejected() {
    let h = make_harness();
    let manifest = zero_capability_manifest();
    let resp = invoke(
        &h,
        &manifest,
        r#"{"subject_ref": "sess-x", "observation": "odd", "severity": "catastrophic"}"#,
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
}

#[test]
fn default_severity_is_medium() {
    let h = make_harness();
    let manifest = zero_capability_manifest();
    let resp = invoke(
        &h,
        &manifest,
        r#"{"subject_ref": "sess-x", "observation": "odd"}"#,
    );
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["severity"], "medium");
}

#[test]
fn flood_cap_rejects_filing_loudly() {
    let h = make_harness();
    h.store.set_anomaly_flag_flood_cap(2);
    let manifest = zero_capability_manifest();

    // Up to the cap, filings succeed through the real tool path.
    for i in 0..2 {
        let resp = invoke(
            &h,
            &manifest,
            &format!(r#"{{"subject_ref": "sess-x-{i}", "observation": "odd"}}"#),
        );
        assert_eq!(resp["ok"], true);
    }

    // The cap+1th filing errors loudly — the tool call fails, no flag row is
    // recorded, and the reporter is told why (never a silent drop).
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();
    let err = registry
        .execute(
            "anomaly_flag",
            &manifest,
            &policy,
            &h.agent_dir,
            None,
            r#"{"subject_ref": "sess-x-2", "observation": "odd"}"#,
            Some("test-session"),
            Some("turn-000003"),
            Some(&gateway_config),
            Some(h.store.clone()),
            None,
        )
        .expect_err("filing beyond the flood cap must error");
    let msg = err.to_string();
    assert!(msg.contains("anomaly_flag_flood"), "got: {msg}");
    assert!(msg.contains("cap 2"), "got: {msg}");

    let pending = h
        .store
        .list_pending_anomaly_flags(Some("witness.default"), 100)
        .expect("query");
    assert_eq!(pending.len(), 2, "rejected filings leave no rows behind");
}
