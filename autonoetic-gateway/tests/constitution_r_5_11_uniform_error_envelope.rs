//! Constitution P-5.11 — native tool failures use a uniform error envelope.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use tempfile::tempdir;

fn no_capability_manifest() -> AgentManifest {
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
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
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
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn invoke(tool_name: &str, args_json: &str) -> anyhow::Result<serde_json::Value> {
    let temp = tempdir()?;
    let manifest = no_capability_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();

    let raw = registry.execute(
        tool_name,
        &manifest,
        &policy,
        temp.path(),
        None,
        args_json,
        Some("session-r-5-11"),
        Some("turn-r-5-11"),
        Some(&gateway_config),
        None,
        None,
    )?;
    Ok(serde_json::from_str(&raw)?)
}

fn assert_error_envelope_shape(payload: &serde_json::Value) {
    assert_eq!(payload["ok"], false, "tool errors must set ok=false");
    assert!(
        payload["error_type"]
            .as_str()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        "error_type must be a non-empty string"
    );
    assert!(
        payload["message"]
            .as_str()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        "message must be a non-empty string"
    );
    // The optional stable `error` code (P-5.11) is a machine token, NOT prose:
    // snake_case `[a-z0-9_]+`. Guards against regressing to the old
    // `"error": "<free-text message>"` shape.
    if let Some(code) = payload.get("error").and_then(|v| v.as_str()) {
        assert!(
            !code.is_empty()
                && code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "`error` must be a snake_case code, not prose: {code:?}"
        );
    }
}

#[test]
fn r_5_11_uniform_error_envelope_contract() -> anyhow::Result<()> {
    let constitution_validation_error = invoke("constitution_read", r#"{"section":"Ri-9.99"}"#)?;
    assert_error_envelope_shape(&constitution_validation_error);

    let user_ask_validation_error = invoke(
        "user_ask",
        r#"{"question":"What is your API key?","context":"Share your secret token."}"#,
    )?;
    assert_error_envelope_shape(&user_ask_validation_error);
    assert!(
        user_ask_validation_error
            .get("repair_hint")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        "expected repair_hint on user.ask secret rejection"
    );

    // Migrated tools (PR #532/#533): every failure path is the canonical envelope.
    // Invoked here without a gateway store / with minimal args, so each returns a
    // structured precondition/validation error — proving none regress to a
    // hand-built `{ "error": "<prose>" }`. (The snake_case `error` check in
    // assert_error_envelope_shape catches the regression.)
    for (tool, args) in [
        ("validation_waive", r#"{"artifact_id":"not-canonical","validation_class":"correctness_check","reason":"x"}"#),
        ("workbench_status", r#"{"workbench_id":"wb_missing"}"#),
        ("planframe_approve", r#"{"plan_id":"plan_missing"}"#),
        ("session_escalate", r#"{"target":"bogus_target","reason":"x","context":"y"}"#),
        ("workflow_wait", r#"{"task_ids":[]}"#),
    ] {
        // Tolerate arg-parse / availability Errs (a separate boundary); only the
        // tool's own returned envelope must be canonical.
        if let Ok(payload) = invoke(tool, args) {
            assert_error_envelope_shape(&payload);
        }
    }

    Ok(())
}
