//! Integration test: sandbox.exec → fails → execution_traces has full error →
//! agent uses execution.search to find the error → gets structured result

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;

fn test_manifest() -> AgentManifest {
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
            id: "coder.default".to_string(),
            name: "coder".to_string(),
            description: "test".to_string(),
        },
        capabilities: vec![
            Capability::CodeExecution {
                patterns: vec!["*".to_string()],
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
        ],
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        middleware: None,
        execution_mode: Default::default(),
        script_entry: None,
        gateway_url: None,
        gateway_token: None,

        response_contract: None,
    }
}

#[test]
fn test_execution_search_finds_past_errors() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let config = GatewayConfig::default();

    // Simulate a failed compilation
    let fail_trace = ExecutionTraceRecord {
        trace_id: "trace-fail-001".to_string(),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: "sess-error-test".to_string(),
        turn_id: Some("turn-001".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox.exec".to_string(),
        command: Some("rustc src/main.rs".to_string()),
        exit_code: Some(1),
        stdout: Some("Compiling...".to_string()),
        stderr: Some(
            "error[E0308]: mismatched types\n\
             --> src/main.rs:42:5\n\
             |\n\
             42 |     let x: i32 = \"hello\";\n\
             |                 ^^^^^^^^^ expected i32, found &str\n\
             "
            .to_string(),
        ),
        duration_ms: 250,
        success: 0,
        error_type: Some("compilation".to_string()),
        error_summary: Some("error[E0308]: mismatched types".to_string()),
        approval_required: None,
        approval_request_id: None,
        arguments: Some(r#"{"command": "rustc src/main.rs"}"#.to_string()),
        result: Some(r#"{"ok": false, "exit_code": 1, "stderr": "error[E0308]: ..."}"#.to_string()),
    };
    store.create_execution_trace(&fail_trace)?;

    // Simulate a successful test run
    let success_trace = ExecutionTraceRecord {
        trace_id: "trace-success-001".to_string(),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: "sess-error-test".to_string(),
        turn_id: Some("turn-002".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox.exec".to_string(),
        command: Some("cargo test".to_string()),
        exit_code: Some(0),
        stdout: Some("running 5 tests\ntest result: ok. 5 passed".to_string()),
        stderr: Some("".to_string()),
        duration_ms: 1500,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: Some(r#"{"command": "cargo test"}"#.to_string()),
        result: Some(r#"{"ok": true, "exit_code": 0}"#.to_string()),
    };
    store.create_execution_trace(&success_trace)?;

    // Now use execution.search tool to find the compilation error
    let registry = autonoetic_gateway::runtime::tools::default_registry();
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    // Search for compilation errors
    let args = serde_json::json!({
        "tool_name": "sandbox.exec",
        "success": false,
        "error_type": "compilation",
        "limit": 10
    });

    let result = registry.execute(
        "execution.search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert!(parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));

    let results = parsed
        .get("results")
        .and_then(|v| v.as_array())
        .expect("results should be an array");
    assert_eq!(results.len(), 1, "Should find one compilation error");

    let error_result = &results[0];
    assert_eq!(
        error_result.get("trace_id").and_then(|v| v.as_str()),
        Some("trace-fail-001")
    );
    assert_eq!(
        error_result.get("error_type").and_then(|v| v.as_str()),
        Some("compilation")
    );
    assert!(
        error_result
            .get("stderr")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("mismatched types"),
        "Should have full error message"
    );
    assert_eq!(
        error_result.get("exit_code").and_then(|v| v.as_i64()),
        Some(1)
    );

    // Search for all sandbox.exec runs
    let args_all = serde_json::json!({
        "tool_name": "sandbox.exec",
        "limit": 100
    });

    let result_all = registry.execute(
        "execution.search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args_all.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed_all: serde_json::Value = serde_json::from_str(&result_all)?;
    let all_results = parsed_all
        .get("results")
        .and_then(|v| v.as_array())
        .expect("results should be an array");
    assert_eq!(all_results.len(), 2, "Should find two sandbox.exec traces");

    Ok(())
}

#[test]
fn test_execution_search_with_command_pattern() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    // Add traces with different commands
    store.create_execution_trace(&ExecutionTraceRecord {
        trace_id: "trace-rust".to_string(),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: "sess-pattern".to_string(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox.exec".to_string(),
        command: Some("rustc main.rs".to_string()),
        exit_code: Some(0),
        stdout: None,
        stderr: None,
        duration_ms: 100,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
    })?;

    store.create_execution_trace(&ExecutionTraceRecord {
        trace_id: "trace-python".to_string(),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: "sess-pattern".to_string(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox.exec".to_string(),
        command: Some("python script.py".to_string()),
        exit_code: Some(0),
        stdout: None,
        stderr: None,
        duration_ms: 200,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
    })?;

    let registry = autonoetic_gateway::runtime::tools::default_registry();
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let config = GatewayConfig::default();

    // Search for rustc commands
    let args = serde_json::json!({
        "command_pattern": "rustc",
        "limit": 100
    });

    let result = registry.execute(
        "execution.search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args.to_string(),
        None,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    let results = parsed.get("results").and_then(|v| v.as_array()).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("trace_id").and_then(|v| v.as_str()),
        Some("trace-rust")
    );

    Ok(())
}
