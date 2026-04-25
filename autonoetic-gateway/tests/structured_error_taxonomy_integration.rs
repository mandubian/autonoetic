//! Integration tests for the structured error taxonomy (Issue #12).
//!
//! Verifies that tool error responses use the ToolError envelope with
//! `ok`, `error_type`, `message`, and `repair_hint` fields, and that
//! LoopGuard correctly classifies permission vs validation errors.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::guard::{LoopGuard, LoopGuardState};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{
    AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::tool_error::ToolError;
use autonoetic_types::tool_error::ToolErrorType;
use serde_json::json;
use tempfile::tempdir;

fn test_manifest(caps: Vec<Capability>) -> AgentManifest {
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
        capabilities: caps,
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
        response_contract: None,
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
}

// ---------------------------------------------------------------------------
// ToolError serialization
// ---------------------------------------------------------------------------

#[test]
fn test_tool_error_serializes_correct_shape() {
    let err = ToolError::permission("access denied");
    let json_str = err.to_error_response();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "permission");
    assert!(parsed["message"].as_str().unwrap().contains("access denied"));
    assert!(parsed["repair_hint"].is_string());
}

#[test]
fn test_tool_error_not_found_includes_repair_hint() {
    let err = ToolError::not_found("credential 'abc'", Some("Use credential.check.".to_string()));
    let json_str = err.to_error_response();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "not_found");
    assert!(parsed["message"].as_str().unwrap().contains("credential 'abc'"));
    assert_eq!(
        parsed["repair_hint"].as_str().unwrap(),
        "Use credential.check."
    );
}

#[test]
fn test_tool_error_validation_has_optional_hint() {
    let err = ToolError::validation("bad input", None::<String>);
    let json_str = err.to_error_response();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "validation");
    assert!(parsed.get("repair_hint").is_none());
}

#[test]
fn test_tool_error_quota_exceeded() {
    let err = ToolError::quota_exceeded(
        "max jobs reached",
        Some("Cancel existing jobs.".to_string()),
    );
    assert_eq!(err.error_type, ToolErrorType::QuotaExceeded);
    assert!(err.is_recoverable());
}

#[test]
fn test_tool_error_conflict() {
    let err = ToolError::conflict("already exists", None::<String>);
    assert_eq!(err.error_type, ToolErrorType::Conflict);
    assert!(err.is_recoverable());
}

#[test]
fn test_tool_error_timeout() {
    let err = ToolError::timeout("request timed out", Some("Retry with backoff.".to_string()));
    assert_eq!(err.error_type, ToolErrorType::Timeout);
    assert!(err.is_recoverable());
}

#[test]
fn test_tool_error_fatal_is_not_recoverable() {
    let err = ToolError::fatal("corrupted state", Some("details".to_string()));
    assert_eq!(err.error_type, ToolErrorType::Fatal);
    assert!(!err.is_recoverable());
}

// ---------------------------------------------------------------------------
// End-to-end: credential.check returns structured error for missing store
// ---------------------------------------------------------------------------

#[test]
fn test_credential_check_no_store_returns_structured_error() {
    let temp = tempdir().unwrap();

    let manifest = test_manifest(vec![Capability::CredentialAccess {
        services: vec!["*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let result = registry
        .execute(
            "credential_check",
            &manifest,
            &policy,
            temp.path(),
            Some(temp.path()),
            &json!({"service": "github"}).to_string(),
            Some("session-1"),
            None,
            None,
            None,
            None,
        )
        .expect("tool call should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "resource");
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("Gateway store not available"));
}

// ---------------------------------------------------------------------------
// End-to-end: scheduler.cron.* returns structured errors
// ---------------------------------------------------------------------------

#[test]
fn test_scheduler_create_no_store_returns_structured_error() {
    let temp = tempdir().unwrap();

    let manifest = test_manifest(vec![Capability::SchedulerAccess {
        patterns: vec!["*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();

    let result = registry
        .execute(
            "scheduler_cron_create",
            &manifest,
            &policy,
            temp.path(),
            None,
            &json!({"message": "tick", "schedule_expr": "every 5 minutes"}).to_string(),
            Some("session-1"),
            None,
            None,
            None,
            None,
        )
        .expect("tool call should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "resource");
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("Gateway store not available"));
}

#[test]
fn test_scheduler_pause_not_found_returns_structured_error() {
    let temp = tempdir().unwrap();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();

    let manifest = test_manifest(vec![Capability::SchedulerAccess {
        patterns: vec!["*".to_string()],
    }]);
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let store = std::sync::Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let result = registry
        .execute(
            "scheduler_cron_pause",
            &manifest,
            &policy,
            temp.path(),
            None,
            &json!({"job_id": "sj-nonexistent"}).to_string(),
            Some("session-1"),
            None,
            None,
            Some(store),
            None,
        )
        .expect("tool call should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["error_type"], "not_found");
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("scheduled job 'sj-nonexistent'"));
}

// ---------------------------------------------------------------------------
// LoopGuard integration with error types
// ---------------------------------------------------------------------------

#[test]
fn test_loop_guard_permission_errors_skip_budget() {
    let mut guard = LoopGuard::new(100);

    for _ in 0..20 {
        guard.register_failure(
            "web_fetch",
            r#"{"url":"https://denied.com"}"#,
            Some(&ToolErrorType::Permission),
        );
    }

    assert!(guard.check_loop().is_ok());
}

#[test]
fn test_loop_guard_validation_errors_count_normally() {
    let mut guard = LoopGuard::new(100);

    for _ in 0..5 {
        guard.register_failure(
            "web_fetch",
            r#"{"url":"https://bad.com"}"#,
            Some(&ToolErrorType::Validation),
        );
    }

    assert!(guard.check_loop().is_err());
}

#[test]
fn test_loop_guard_mixed_errors_permission_skipped() {
    let mut guard = LoopGuard::new(100);

    for _ in 0..4 {
        guard.register_failure("web_fetch", "{}", Some(&ToolErrorType::Validation));
        guard.register_failure("web_fetch", "{}", Some(&ToolErrorType::Permission));
    }

    assert!(guard.check_loop().is_ok());

    guard.register_failure("web_fetch", "{}", Some(&ToolErrorType::Validation));
    assert!(guard.check_loop().is_err());
}

#[test]
fn test_loop_guard_unknown_error_type_counts_as_failure() {
    let mut guard = LoopGuard::new(100);

    for _ in 0..5 {
        guard.register_failure("sandbox_exec", "{}", None);
    }

    assert!(guard.check_loop().is_err());
}

#[test]
fn test_loop_guard_snapshot_restore_preserves_failure_counts() {
    let mut guard = LoopGuard::new(100);
    guard.register_failure("web_fetch", "{}", Some(&ToolErrorType::Execution));
    guard.register_failure("web_fetch", "{}", Some(&ToolErrorType::Permission));
    guard.register_failure("sandbox_exec", "{}", None);

    let state: LoopGuardState = guard.snapshot();
    let restored = LoopGuard::restore(state);

    assert_eq!(*restored.snapshot().tool_failure_counts.get("web_fetch").unwrap(), 1);
    assert_eq!(*restored.snapshot().tool_failure_counts.get("sandbox_exec").unwrap(), 1);
}
