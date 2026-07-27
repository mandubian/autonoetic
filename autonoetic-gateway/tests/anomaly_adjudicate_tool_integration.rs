//! `anomaly_adjudicate` — native ombudsman adjudication (citizenship RFC Part
//! F follow-up, #774). Mirrors the harness shape of
//! `anomaly_flag_integration.rs`.
//!
//! Verifies:
//!   - the tool is gated by the `AnomalyAdjudicate` capability (absent → not
//!     available; present → available)
//!   - a happy-path adjudication moves the flag to the requested status and
//!     records `decided_by` = the calling agent
//!   - a `anomaly_flag.decided` causal event with `enforced_rules: ["O-7"]`
//!     is emitted for terminal decisions, and `anomaly_flag.review_started`
//!     for `under_review`
//!   - terminal decisions require a non-empty `reason` (decider-obligation
//!     parity with `anomaly.resolve`)
//!   - terminal decisions require an *exact* pattern grant: a broad `*`
//!     capability unlocks only `under_review`
//!   - `anomaly_adjudication.require_terminal_cosign = true` defers terminal
//!     decisions back to the operator

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::anomaly_flags::AnomalyFlag;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{AnomalyAdjudicationConfig, GatewayConfig};
use std::sync::Arc;
use tempfile::tempdir;

fn manifest_with(caps: Vec<Capability>) -> AgentManifest {
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
            id: "ombudsman.default".to_string(),
            name: "ombudsman".to_string(),
            description: "test office".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: caps,
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

/// Full terminal grant (what the ombudsman bundle ships).
fn office_manifest() -> AgentManifest {
    manifest_with(vec![Capability::AnomalyAdjudicate {
        patterns: vec![
            "under_review".to_string(),
            "confirmed".to_string(),
            "dismissed".to_string(),
            "deferred".to_string(),
        ],
    }])
}

/// Broad participation grant — must NOT unlock terminal decisions.
fn wildcard_manifest() -> AgentManifest {
    manifest_with(vec![Capability::AnomalyAdjudicate {
        patterns: vec!["*".to_string()],
    }])
}

struct Harness {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    agent_dir: std::path::PathBuf,
}

fn make_harness() -> Harness {
    let temp = tempdir().expect("tempdir");
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("ombudsman.default");
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

fn seed_flag(h: &Harness, flag_id: &str) {
    let flag = AnomalyFlag {
        flag_id: flag_id.to_string(),
        reporter_agent_id: "witness.default".to_string(),
        reporter_session_id: Some("sess-w1".to_string()),
        subject_ref: "sess-suspect".to_string(),
        observation: "odd tool call".to_string(),
        evidence_json: serde_json::json!(["evt-1"]),
        severity: "high".to_string(),
        status: "pending".to_string(),
        decision: None,
        decision_reason: None,
        decided_by: None,
        decided_at: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        sla_breached_at: None,
    };
    h.store.insert_anomaly_flag(&flag).expect("seed flag");
}

fn invoke(
    h: &Harness,
    manifest: &AgentManifest,
    args_json: &str,
    config: &GatewayConfig,
) -> serde_json::Value {
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let raw = registry
        .execute(
            "anomaly_adjudicate",
            manifest,
            &policy,
            &h.agent_dir,
            None,
            args_json,
            Some("office-session"),
            Some("turn-000001"),
            Some(config),
            Some(h.store.clone()),
            None,
        )
        .expect("execute should not error for normal cases");
    serde_json::from_str(&raw).expect("response is JSON")
}

fn default_cfg() -> GatewayConfig {
    GatewayConfig::default()
}

fn cosign_cfg() -> GatewayConfig {
    let mut cfg = GatewayConfig::default();
    cfg.anomaly_adjudication = AnomalyAdjudicationConfig {
        require_terminal_cosign: true,
    };
    cfg
}

#[test]
fn tool_hidden_without_capability() {
    let registry = default_registry();
    let manifest = manifest_with(vec![]);
    let defs = registry.available_definitions(&manifest);
    assert!(
        !defs.iter().any(|d| d.name == "anomaly_adjudicate"),
        "anomaly_adjudicate must NOT be available without AnomalyAdjudicate capability"
    );
}

#[test]
fn tool_visible_with_capability() {
    let registry = default_registry();
    let defs = registry.available_definitions(&office_manifest());
    assert!(
        defs.iter().any(|d| d.name == "anomaly_adjudicate"),
        "anomaly_adjudicate must be available with AnomalyAdjudicate capability"
    );
}

#[test]
fn terminal_decision_records_decided_by_and_event() {
    let h = make_harness();
    seed_flag(&h, "aflag-term-1");
    let manifest = office_manifest();

    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-term-1", "status": "confirmed", "reason": "evidence corroborated"}"#,
        &default_cfg(),
    );

    assert_eq!(resp["ok"], true);
    assert_eq!(resp["status"], "confirmed");
    assert_eq!(resp["terminal"], true);
    assert_eq!(resp["decided_by"], "ombudsman.default");

    let row = h
        .store
        .get_anomaly_flag("aflag-term-1")
        .expect("query")
        .expect("row exists");
    assert_eq!(row.status, "confirmed");
    assert_eq!(row.decision.as_deref(), Some("confirmed"));
    assert_eq!(row.decided_by.as_deref(), Some("ombudsman.default"));
    assert_eq!(row.decision_reason.as_deref(), Some("evidence corroborated"));
    assert!(row.decided_at.is_some());

    let events = h
        .store
        .search_causal_events(Some("office-session"), None, 100)
        .expect("query causal events");
    let decided = events
        .iter()
        .find(|e| e.category == "anomaly_flag" && e.action == "decided")
        .expect("anomaly_flag.decided causal event exists");
    assert_eq!(decided.enforced_rules, vec!["O-7".to_string()]);
    assert_eq!(decided.target.as_deref(), Some("aflag-term-1"));
    assert_eq!(decided.status, "confirmed");
}

#[test]
fn under_review_emits_review_started_event_and_leaves_decision_null() {
    let h = make_harness();
    seed_flag(&h, "aflag-review-1");
    let manifest = office_manifest();

    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-review-1", "status": "under_review"}"#,
        &default_cfg(),
    );

    assert_eq!(resp["ok"], true);
    assert_eq!(resp["status"], "under_review");
    assert_eq!(resp["terminal"], false);

    let row = h.store.get_anomaly_flag("aflag-review-1").unwrap().unwrap();
    assert_eq!(row.status, "under_review");
    assert!(row.decision.is_none(), "under_review must not stamp decision");
    assert!(row.decided_by.is_none());

    let events = h
        .store
        .search_causal_events(Some("office-session"), None, 100)
        .expect("query");
    assert!(events.iter().any(|e| {
        e.category == "anomaly_flag" && e.action == "review_started"
    }));
}

#[test]
fn terminal_decision_requires_reason() {
    let h = make_harness();
    seed_flag(&h, "aflag-noreason");
    let manifest = office_manifest();

    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-noreason", "status": "dismissed"}"#,
        &default_cfg(),
    );

    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
    assert!(
        resp["message"]
            .as_str()
            .unwrap_or_default()
            .contains("non-empty reason"),
        "got: {resp}"
    );

    // The flag is untouched — no silent partial update.
    let row = h.store.get_anomaly_flag("aflag-noreason").unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert!(row.decided_at.is_none());
}

#[test]
fn wildcard_grant_does_not_unlock_terminal_decisions() {
    let h = make_harness();
    seed_flag(&h, "aflag-wild-1");
    seed_flag(&h, "aflag-wild-2");
    let manifest = wildcard_manifest();

    // Terminal decision rejected under a broad grant — authority-op discipline.
    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-wild-1", "status": "confirmed", "reason": "x"}"#,
        &default_cfg(),
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "permission");
    assert!(
        resp["message"]
            .as_str()
            .unwrap_or_default()
            .contains("exact pattern grant"),
        "got: {resp}"
    );

    // Non-terminal transition still allowed under `*`.
    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-wild-2", "status": "under_review"}"#,
        &default_cfg(),
    );
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["status"], "under_review");
}

#[test]
fn require_terminal_cosign_defers_to_operator() {
    let h = make_harness();
    seed_flag(&h, "aflag-cosign-1");
    let manifest = office_manifest();

    // Even with an exact terminal grant, co-sign mode defers to the operator.
    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-cosign-1", "status": "confirmed", "reason": "x"}"#,
        &cosign_cfg(),
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "permission");
    assert!(
        resp["message"]
            .as_str()
            .unwrap_or_default()
            .contains("operator co-sign"),
        "got: {resp}"
    );

    // Non-terminal still fine under co-sign mode.
    seed_flag(&h, "aflag-cosign-2");
    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-cosign-2", "status": "under_review"}"#,
        &cosign_cfg(),
    );
    assert_eq!(resp["ok"], true);
}

#[test]
fn unknown_flag_rejected_loudly() {
    let h = make_harness();
    let manifest = office_manifest();

    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-nope", "status": "under_review"}"#,
        &default_cfg(),
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "not_found");
}

#[test]
fn invalid_status_rejected() {
    let h = make_harness();
    seed_flag(&h, "aflag-badstatus");
    let manifest = office_manifest();

    let resp = invoke(
        &h,
        &manifest,
        r#"{"flag_id": "aflag-badstatus", "status": "rejected"}"#,
        &default_cfg(),
    );
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
}
