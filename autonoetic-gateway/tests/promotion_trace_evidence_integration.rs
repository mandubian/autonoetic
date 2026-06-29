//! Trace-based promotion evidence (#580).

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::content_store::ContentStore;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use std::sync::Arc;
use tempfile::tempdir;

fn unit_test_runner_manifest() -> AgentManifest {
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
            id: "unit_test_runner.default".to_string(),
            name: "unit_test_runner.default".to_string(),
            description: "test".to_string(),
            singleton: false,
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
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn auditor_manifest() -> AgentManifest {
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
            id: "auditor.default".to_string(),
            name: "auditor.default".to_string(),
            description: "test".to_string(),
            singleton: false,
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
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn setup_gateway() -> (tempfile::TempDir, std::path::PathBuf, Arc<GatewayStore>) {
    let temp = tempdir().unwrap();
    let gw = temp.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    let cs = ContentStore::new(&gw).unwrap();
    let _ = cs.write(b"artifact".as_slice()).unwrap();
    let store = Arc::new(GatewayStore::open(&gw).unwrap());
    (temp, gw, store)
}

fn invoke_promotion_record(
    gw: &std::path::Path,
    store: Arc<GatewayStore>,
    manifest: &AgentManifest,
    args: serde_json::Value,
    session_id: &str,
) -> serde_json::Value {
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let raw = registry
        .execute(
            "promotion_record",
            manifest,
            &policy,
            gw.parent().unwrap(),
            Some(gw),
            &args.to_string(),
            Some(session_id),
            None,
            None,
            Some(store),
            None,
        )
        .expect("execute returns");
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn execution_role_without_trace_is_rejected() {
    let (_temp, gw, store) = setup_gateway();
    let manifest = unit_test_runner_manifest();
    let result = invoke_promotion_record(
        &gw,
        store,
        &manifest,
        serde_json::json!({
            "artifact_id": "art_trace_test",
            "role": "unit_test_runner",
            "pass": true,
            "findings": [],
        }),
        "session-trace-missing",
    );
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"], "missing_execution_evidence");
}

#[test]
fn execution_role_with_failed_trace_records_pass_false() {
    let (_temp, gw, store) = setup_gateway();
    let session = "session-trace-fail";
    support::promotion_trace::seed_execution_trace(store.as_ref(), session, "trace-fail-001", 1);
    let manifest = unit_test_runner_manifest();
    let result = invoke_promotion_record(
        &gw,
        store,
        &manifest,
        serde_json::json!({
            "artifact_id": "art_trace_fail",
            "role": "unit_test_runner",
            "execution_trace_id": "trace-fail-001",
            "findings": [{"severity": "warning", "description": "tests failed"}],
        }),
        session,
    );
    assert_eq!(result["ok"], true, "{result:?}");
    assert_eq!(result["pass"], false);
}

#[test]
fn execution_role_with_success_trace_records_pass_true() {
    let (_temp, gw, store) = setup_gateway();
    let session = "session-trace-pass";
    support::promotion_trace::seed_success_trace(store.as_ref(), session, "trace-pass-001");
    let manifest = unit_test_runner_manifest();
    let result = invoke_promotion_record(
        &gw,
        store,
        &manifest,
        serde_json::json!({
            "artifact_id": "art_trace_pass",
            "role": "unit_test_runner",
            "execution_trace_id": "trace-pass-001",
            "findings": [],
        }),
        session,
    );
    assert_eq!(result["ok"], true, "{result:?}");
    assert_eq!(result["pass"], true);
}

#[test]
fn llm_pass_true_without_trace_cannot_fake_success() {
    let (_temp, gw, store) = setup_gateway();
    let manifest = unit_test_runner_manifest();
    let result = invoke_promotion_record(
        &gw,
        store,
        &manifest,
        serde_json::json!({
            "artifact_id": "art_weather_fake",
            "role": "sealed_evaluator",
            "pass": true,
            "findings": [],
            "summary": "42/42 mocked tests passed"
        }),
        "session-weather-fake",
    );
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"], "missing_execution_evidence");
}

#[test]
fn warning_findings_do_not_block_static_evaluator_pass() {
    let (_temp, gw, store) = setup_gateway();
    let session = "session-warning-advisory";
    let manifest = unit_test_runner_manifest();
    let result = invoke_promotion_record(
        &gw,
        store,
        &manifest,
        serde_json::json!({
            "artifact_id": "art_warning_ok",
            "role": "static_evaluator",
            "pass": true,
            "findings": [{"severity": "warning", "description": "style nit", "evidence": ""}],
        }),
        session,
    );
    assert_eq!(result["ok"], true, "{result:?}");
    assert_eq!(result["pass"], true);
}

#[test]
fn auditor_critical_findings_veto_pass() {
    let (_temp, gw, store) = setup_gateway();
    let manifest = auditor_manifest();
    let result = invoke_promotion_record(
        &gw,
        store,
        &manifest,
        serde_json::json!({
            "artifact_id": "art_auditor_veto",
            "role": "auditor",
            "pass": true,
            "findings": [{
                "severity": "critical",
                "description": "hardcoded API key",
                "evidence": "api_key = 'sk-live-...'"
            }],
        }),
        "session-auditor-veto",
    );
    assert_eq!(result["ok"], true, "{result:?}");
    assert_eq!(result["pass"], false);
}

#[test]
fn auditor_non_critical_findings_are_advisory() {
    let (_temp, gw, store) = setup_gateway();
    let manifest = auditor_manifest();
    let result = invoke_promotion_record(
        &gw,
        store,
        &manifest,
        serde_json::json!({
            "artifact_id": "art_auditor_advisory",
            "role": "auditor",
            "pass": true,
            "findings": [{
                "severity": "warning",
                "description": "missing docstring"
            }],
        }),
        "session-auditor-advisory",
    );
    assert_eq!(result["ok"], true, "{result:?}");
    assert_eq!(result["pass"], true);
}
