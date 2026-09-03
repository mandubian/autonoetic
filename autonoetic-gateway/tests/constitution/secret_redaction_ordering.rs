//! Constitution R+9: Redaction-before-write ordering invariant.
//!
//! R+9 guarantees that redaction runs **before** causal-chain append on every
//! path that can contain secret-shaped content. This is enforced at the type
//! level: `CausalLogger::log()` and `log_durable()` accept only
//! `Option<RedactedPayload>`, making it a compile error to pass raw strings.
//!
//! Tests:
//! - Secret-shaped tool arguments never appear raw in the JSONL on disk.
//! - Secret-shaped payload values are redacted before reaching the log file.
//! - `RedactedPayload::from_raw()` redacts bearer tokens, API keys, and
//!   env-var assignments.


use autonoetic_gateway::causal_chain::CausalLogger;
use autonoetic_gateway::log_redaction::RedactedPayload;
use autonoetic_types::causal_chain::EntryStatus;
use tempfile::tempdir;

/// Redaction-before-write now means: the JSONL carries no payload at all, and
/// the content-addressed copy under `history/payloads/` holds only redacted
/// bytes (#1278). This reads that copy back for assertion.
fn cas_payload_for_last_entry(path: &std::path::Path) -> String {
    let entries = CausalLogger::read_entries(path).unwrap();
    let entry = entries.last().expect("at least one entry");
    let reference = entry
        .payload_ref
        .as_deref()
        .expect("lean entry should reference its payload");
    std::fs::read_to_string(
        path.parent()
            .unwrap()
            .join("payloads")
            .join(format!("{reference}.json")),
    )
    .expect("CAS payload should exist")
}

#[test]
fn r9_bearer_token_never_appears_raw_in_jsonl() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = {
        let history = agent_dir.join("history");
        std::fs::create_dir_all(&history).unwrap();
        history.join("causal_chain.jsonl")
    };

    let logger = CausalLogger::new(&path).unwrap();
    let secret_token = "Bearer super-secret-token-abc123";

    logger
        .log(
            "test-agent",
            "session-r9-bearer",
            None,
            0,
            "tool",
            "sandbox_exec",
            EntryStatus::Success,
            None,
            &autonoetic_types::causal_chain::default_enforced_rules(),
            Some(RedactedPayload::from_raw(serde_json::json!({
                "command": format!("curl -H 'Authorization: {}' https://api.example.com", secret_token)
            }))),
        )
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        !on_disk.contains(secret_token),
        "R+9 violation: raw bearer token found in JSONL"
    );
    let cas_payload = cas_payload_for_last_entry(&path);
    assert!(
        !cas_payload.contains(secret_token),
        "R+9 violation: raw bearer token found in the content-addressed payload"
    );
    assert!(
        cas_payload.contains("***REDACTED***"),
        "redacted placeholder should be present in the content-addressed payload"
    );
}

#[test]
fn r9_api_key_env_assignment_never_appears_raw() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = {
        let history = agent_dir.join("history");
        std::fs::create_dir_all(&history).unwrap();
        history.join("causal_chain.jsonl")
    };

    let logger = CausalLogger::new(&path).unwrap();
    let secret_value = "sk-testplaceholder_not_a_real_key_0000";

    logger
        .log(
            "test-agent",
            "session-r9-env",
            None,
            0,
            "tool",
            "sandbox_exec",
            EntryStatus::Success,
            None,
            &autonoetic_types::causal_chain::default_enforced_rules(),
            Some(RedactedPayload::from_raw(serde_json::json!({
                "command": format!("export OPENAI_API_KEY={} && python3 app.py", secret_value)
            }))),
        )
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        !on_disk.contains(secret_value),
        "R+9 violation: raw API key found in JSONL"
    );
    assert!(
        !cas_payload_for_last_entry(&path).contains(secret_value),
        "R+9 violation: raw API key found in the content-addressed payload"
    );
}

#[test]
fn r9_query_param_secret_never_appears_raw() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = {
        let history = agent_dir.join("history");
        std::fs::create_dir_all(&history).unwrap();
        history.join("causal_chain.jsonl")
    };

    let logger = CausalLogger::new(&path).unwrap();
    let secret_param = "testplaceholder_not_a_real_key_0000";

    logger
        .log(
            "test-agent",
            "session-r9-query",
            None,
            0,
            "tool",
            "sandbox_exec",
            EntryStatus::Success,
            None,
            &autonoetic_types::causal_chain::default_enforced_rules(),
            Some(RedactedPayload::from_raw(serde_json::json!({
                "url": format!("https://api.example.com/data?api_key={}&q=test", secret_param)
            }))),
        )
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        !on_disk.contains(secret_param),
        "R+9 violation: raw API key in query param found in JSONL"
    );
    assert!(
        !cas_payload_for_last_entry(&path).contains(secret_param),
        "R+9 violation: query-param key found in the content-addressed payload"
    );
}

#[test]
fn r9_json_sensitive_key_value_redacted() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = {
        let history = agent_dir.join("history");
        std::fs::create_dir_all(&history).unwrap();
        history.join("causal_chain.jsonl")
    };

    let logger = CausalLogger::new(&path).unwrap();

    logger
        .log(
            "test-agent",
            "session-r9-json",
            None,
            0,
            "tool",
            "http_request",
            EntryStatus::Success,
            None,
            &autonoetic_types::causal_chain::default_enforced_rules(),
            Some(RedactedPayload::from_raw(serde_json::json!({
                "token": "my-secret-token-value",
                "password": "hunter2",
                "safe_field": "this is fine"
            }))),
        )
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        !on_disk.contains("my-secret-token-value"),
        "R+9 violation: raw token value found in JSONL"
    );
    assert!(
        !on_disk.contains("hunter2"),
        "R+9 violation: raw password value found in JSONL"
    );
    let cas_payload = cas_payload_for_last_entry(&path);
    assert!(
        !cas_payload.contains("my-secret-token-value"),
        "R+9 violation: raw token value found in the content-addressed payload"
    );
    assert!(
        !cas_payload.contains("hunter2"),
        "R+9 violation: raw password value found in the content-addressed payload"
    );
    assert!(
        cas_payload.contains("this is fine"),
        "non-sensitive fields should be preserved in the content-addressed payload"
    );
}

#[test]
fn r9_from_redacted_passes_through_unmodified() {
    let payload = serde_json::json!({"reason": "agent_exit", "safe": true});
    let rp = RedactedPayload::from_redacted(payload.clone());
    assert_eq!(rp.into_inner(), payload);
}

#[test]
fn r9_durable_path_also_redacts() {
    let temp = tempdir().unwrap();
    let agent_dir = temp.path().join("agent");
    let path = {
        let history = agent_dir.join("history");
        std::fs::create_dir_all(&history).unwrap();
        history.join("causal_chain.jsonl")
    };

    let logger = CausalLogger::new(&path).unwrap();

    logger
        .log_durable(
            "evaluator.default",
            "session-r9-durable",
            None,
            0,
            "tool",
            "promotion_record",
            EntryStatus::Success,
            None,
            &autonoetic_types::causal_chain::default_enforced_rules(),
            Some(RedactedPayload::from_raw(serde_json::json!({
                "arguments": {
                    "api_key": "sk-secret-key-for-testing-only",
                    "artifact_id": "art_test123"
                }
            }))),
        )
        .unwrap();

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        !on_disk.contains("sk-secret-key-for-testing-only"),
        "R+9 violation: log_durable path leaked secret"
    );
    let cas_payload = cas_payload_for_last_entry(&path);
    assert!(
        !cas_payload.contains("sk-secret-key-for-testing-only"),
        "R+9 violation: log_durable path leaked secret into the content-addressed payload"
    );
    assert!(
        cas_payload.contains("art_test123"),
        "non-sensitive fields should be preserved in durable path"
    );
}
