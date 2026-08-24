//! Integration test: sandbox.exec → fails → execution_traces has full error →
//! agent uses execution_search to find the error → gets structured result
//!
//! Also pins the ownership scope (#1062): the caller's root session is both the
//! default and the ceiling for `session_id`. Before that, omitting `session_id`
//! searched every session in the store and returned other roots' raw `stdout`
//! verbatim — and every agent has this tool (`is_available` is unconditional).


use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use autonoetic_types::causal_chain::ExecutionTraceRecord;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn test_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "coder.default".to_string(),
            name: "coder".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![
            Capability::CodeExecution {
                patterns: vec!["*".to_string()],
                commands: vec![],
            },
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
        ],
        ..TestManifest::new().build()
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
        tool_name: "sandbox_exec".to_string(),
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
        egress_label: None,
        mount_set: None,
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
        tool_name: "sandbox_exec".to_string(),
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
        egress_label: None,
        mount_set: None,
    };
    store.create_execution_trace(&success_trace)?;

    // Now use execution_search tool to find the compilation error
    let registry = autonoetic_gateway::runtime::tools::default_registry();
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());

    // Search for compilation errors
    let args = serde_json::json!({
        "tool_name": "sandbox_exec",
        "success": false,
        "error_type": "compilation",
        "limit": 10
    });

    let result = registry.execute(
        "execution_search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args.to_string(),
        Some("sess-error-test"),
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
        "tool_name": "sandbox_exec",
        "limit": 100
    });

    let result_all = registry.execute(
        "execution_search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args_all.to_string(),
        Some("sess-error-test"),
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
        tool_name: "sandbox_exec".to_string(),
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
        egress_label: None,
        mount_set: None,
    })?;

    store.create_execution_trace(&ExecutionTraceRecord {
        trace_id: "trace-python".to_string(),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: "sess-pattern".to_string(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox_exec".to_string(),
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
        egress_label: None,
        mount_set: None,
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
        "execution_search",
        &manifest,
        &policy,
        temp.path(),
        Some(&gateway_dir),
        &args.to_string(),
        Some("sess-pattern"),
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

// ── ownership scope (#1062) ─────────────────────────────────────────────────

/// A minimal trace in `session_id` whose stdout is a distinctive canary.
fn trace_in(session_id: &str, trace_id: &str, stdout: &str) -> ExecutionTraceRecord {
    ExecutionTraceRecord {
        trace_id: trace_id.to_string(),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: session_id.to_string(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox_exec".to_string(),
        command: Some("python3 fetch.py".to_string()),
        exit_code: Some(0),
        stdout: Some(stdout.to_string()),
        stderr: None,
        duration_ms: 10,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: None,
        approval_request_id: None,
        arguments: None,
        result: None,
        egress_label: None,
        mount_set: None,
    }
}

/// Two roots, one trace each, plus a child session under root A.
fn two_root_store(gateway_dir: &std::path::Path) -> anyhow::Result<Arc<GatewayStore>> {
    let store = Arc::new(GatewayStore::open(gateway_dir)?);
    store.create_execution_trace(&trace_in("root-a", "trace-a", "MINE"))?;
    store.create_execution_trace(&trace_in("root-a/child-1", "trace-a-child", "MINE-CHILD"))?;
    store.create_execution_trace(&trace_in("root-b", "trace-b", "OTHER-OPERATOR-SECRET"))?;
    Ok(store)
}

fn search(
    store: &Arc<GatewayStore>,
    gateway_dir: &std::path::Path,
    temp: &std::path::Path,
    session_id: Option<&str>,
    args: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let registry = autonoetic_gateway::runtime::tools::default_registry();
    let manifest = test_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let config = GatewayConfig::default();
    let raw = registry.execute(
        "execution_search",
        &manifest,
        &policy,
        temp,
        Some(gateway_dir),
        &args.to_string(),
        session_id,
        None,
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    Ok(serde_json::from_str(&raw)?)
}

fn trace_ids(parsed: &serde_json::Value) -> Vec<String> {
    parsed
        .get("results")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("trace_id").and_then(|v| v.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The core of #1062: an omitted `session_id` no longer means "every session in
/// the store". It means the caller's own root — so another root's raw stdout is
/// not reachable at all, whatever the sink would have allowed.
#[test]
fn execution_search_defaults_to_the_callers_root_session() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = two_root_store(&gateway_dir)?;

    let parsed = search(
        &store,
        &gateway_dir,
        temp.path(),
        Some("root-a"),
        serde_json::json!({ "limit": 100 }),
    )?;

    let mut ids = trace_ids(&parsed);
    ids.sort();
    assert_eq!(ids, vec!["trace-a", "trace-a-child"]);
    assert!(
        !parsed.to_string().contains("OTHER-OPERATOR-SECRET"),
        "another root's stdout must not be reachable: {parsed}"
    );
    assert_eq!(
        parsed.get("session_scope").and_then(|v| v.as_str()),
        Some("root-a"),
        "the response must say what was actually searched"
    );
    Ok(())
}

/// A child session searches its whole root, not just itself — peers under one
/// root are one trust domain (#1061), which is the boundary this scope encodes.
#[test]
fn execution_search_from_a_child_session_scopes_to_its_root() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = two_root_store(&gateway_dir)?;

    let parsed = search(
        &store,
        &gateway_dir,
        temp.path(),
        Some("root-a/child-1"),
        serde_json::json!({ "limit": 100 }),
    )?;

    let mut ids = trace_ids(&parsed);
    ids.sort();
    assert_eq!(ids, vec!["trace-a", "trace-a-child"]);
    Ok(())
}

/// An explicit `session_id` may narrow within the caller's root.
#[test]
fn execution_search_allows_narrowing_within_the_callers_root() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = two_root_store(&gateway_dir)?;

    let parsed = search(
        &store,
        &gateway_dir,
        temp.path(),
        Some("root-a"),
        serde_json::json!({ "session_id": "root-a/child-1", "limit": 100 }),
    )?;

    assert_eq!(trace_ids(&parsed), vec!["trace-a-child"]);
    Ok(())
}

/// …but never widen past it. An explicit foreign root is refused rather than
/// silently returning nothing, so the boundary is legible to the agent.
#[test]
fn execution_search_refuses_a_session_outside_the_callers_root() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = two_root_store(&gateway_dir)?;

    let parsed = search(
        &store,
        &gateway_dir,
        temp.path(),
        Some("root-a"),
        serde_json::json!({ "session_id": "root-b", "limit": 100 }),
    )?;

    assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(false));
    assert!(
        !parsed.to_string().contains("OTHER-OPERATOR-SECRET"),
        "refusal must not leak the other root's content: {parsed}"
    );
    Ok(())
}

/// A prefix of the root is not the root: `root-a` must not open up `root-ab`.
#[test]
fn execution_search_refuses_a_root_that_merely_shares_a_prefix() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    store.create_execution_trace(&trace_in("root-a", "trace-a", "MINE"))?;
    store.create_execution_trace(&trace_in("root-ab", "trace-ab", "NEIGHBOUR-SECRET"))?;

    let parsed = search(
        &store,
        &gateway_dir,
        temp.path(),
        Some("root-a"),
        serde_json::json!({ "session_id": "root-ab", "limit": 100 }),
    )?;
    assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(false));

    // And the default scope must not sweep it in either.
    let defaulted = search(
        &store,
        &gateway_dir,
        temp.path(),
        Some("root-a"),
        serde_json::json!({ "limit": 100 }),
    )?;
    assert_eq!(trace_ids(&defaulted), vec!["trace-a"]);
    Ok(())
}

/// No establishable caller ⇒ refuse. This tool is available to every agent and
/// survives the clarification/degraded tier filters, so an unscoped fallback
/// would be reachable from every session in the system.
#[test]
fn execution_search_refuses_without_a_session_context() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = two_root_store(&gateway_dir)?;

    let parsed = search(
        &store,
        &gateway_dir,
        temp.path(),
        None,
        serde_json::json!({ "limit": 100 }),
    )?;

    assert_eq!(parsed.get("ok").and_then(|v| v.as_bool()), Some(false));
    assert!(
        !parsed.to_string().contains("OTHER-OPERATOR-SECRET")
            && !parsed.to_string().contains("MINE"),
        "an unscoped call must return no trace content: {parsed}"
    );
    Ok(())
}

/// #1002 slice 1: the gateway-asserted mount set persists and round-trips
/// through the store — the after-the-fact answer to "what could this exec
/// see?". A `None` mount set (pre-v81 rows, non-sandbox tools) stays `None`.
#[test]
fn test_execution_trace_mount_set_roundtrip() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);

    let with_mounts = ExecutionTraceRecord {
        trace_id: "trace-mounts-001".to_string(),
        event_id: None,
        agent_id: "coder.default".to_string(),
        session_id: "sess-mounts".to_string(),
        turn_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        tool_name: "sandbox_exec".to_string(),
        command: Some("cat notes.md".to_string()),
        exit_code: Some(0),
        stdout: None,
        stderr: None,
        duration_ms: 12,
        success: 1,
        error_type: None,
        error_summary: None,
        approval_required: Some(0),
        approval_request_id: None,
        arguments: None,
        result: None,
        egress_label: None,
        mount_set: Some(vec![
            "ro:host_root".to_string(),
            "rw:/agents/coder".to_string(),
            "ro:/mail/inbox.mbox".to_string(),
        ]),
    };
    store.create_execution_trace(&with_mounts)?;

    let read = store
        .get_execution_trace("trace-mounts-001")?
        .expect("trace row must exist");
    assert_eq!(
        read.mount_set,
        Some(vec![
            "ro:host_root".to_string(),
            "rw:/agents/coder".to_string(),
            "ro:/mail/inbox.mbox".to_string(),
        ]),
        "mount_set must survive the store roundtrip verbatim"
    );
    Ok(())
}
