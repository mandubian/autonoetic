//! Constitution R+2 / R-5.13 — Egress schema validation on tool results.
//!
//! Child → parent tool results validate against `io.returns` on egress,
//! symmetric to ingress `io.accepts`.  Fail-open for missing schema,
//! fail-closed for declared-but-violated schema.

mod support;

use autonoetic_gateway::execution::SpawnResult;
use autonoetic_gateway::runtime::response_validation::{
    parse_response_contract, validate_spawn_response,
};
use autonoetic_types::agent::ResponseContract;

fn minimal_result(reply: &str) -> SpawnResult {
    SpawnResult {
        agent_id: "child-agent".to_string(),
        session_id: "sess-1".to_string(),
        assistant_reply: Some(reply.to_string()),
        should_signal_background: false,
        artifacts: vec![],
        files: vec![],
        shared_knowledge: vec![],
        llm_usage: vec![],
        suspended_for_approval: None,
        suspended_for_user_input: false,
    }
}

fn contract_with_output_schema(schema: serde_json::Value) -> ResponseContract {
    ResponseContract {
        output_schema: Some(schema),
        ..ResponseContract::default()
    }
}

#[test]
fn egress_rejects_missing_required_field() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["status", "count"],
        "properties": {
            "status": { "type": "string" },
            "count": { "type": "integer" }
        }
    });
    let contract = contract_with_output_schema(schema);
    let result = minimal_result(r#"{"status": "ok"}"#);

    let violations = validate_spawn_response(&result, &contract, None);
    assert!(!violations.is_empty(), "should violate missing 'count'");
    assert!(
        violations.iter().any(|v| v.message.contains("count")),
        "{:?}",
        violations
    );
}

#[test]
fn egress_rejects_wrong_type() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["count"],
        "properties": {
            "count": { "type": "integer" }
        }
    });
    let contract = contract_with_output_schema(schema);
    let result = minimal_result(r#"{"count": "not-a-number"}"#);

    let violations = validate_spawn_response(&result, &contract, None);
    assert!(!violations.is_empty(), "should violate type");
    assert!(
        violations
            .iter()
            .any(|v| v.message.contains("expected type")),
        "{:?}",
        violations
    );
}

#[test]
fn egress_rejects_invalid_json_when_schema_constrained() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["status"],
        "properties": {
            "status": { "type": "string" }
        }
    });
    let contract = contract_with_output_schema(schema);
    let result = minimal_result("this is not json");

    let violations = validate_spawn_response(&result, &contract, None);
    assert!(!violations.is_empty(), "should violate invalid JSON");
    assert!(
        violations
            .iter()
            .any(|v| v.message.contains("not valid JSON")),
        "{:?}",
        violations
    );
}

#[test]
fn egress_passes_valid_response() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["status"],
        "properties": {
            "status": { "type": "string" },
            "data": { "type": "object" }
        }
    });
    let contract = contract_with_output_schema(schema);
    let result = minimal_result(r#"{"status": "ok", "data": {"x": 1}}"#);

    let violations = validate_spawn_response(&result, &contract, None);
    assert!(
        violations.is_empty(),
        "valid response should pass: {:?}",
        violations
    );
}

#[test]
fn egress_no_schema_means_no_violations() {
    let contract = ResponseContract::default();
    let result = minimal_result("anything at all");

    let violations = validate_spawn_response(&result, &contract, None);
    assert!(
        violations.is_empty(),
        "no output_schema should pass everything: {:?}",
        violations
    );
}

#[test]
fn egress_parse_contract_from_metadata() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["result"],
        "properties": {
            "result": { "type": "string" }
        }
    });
    let metadata = serde_json::json!({
        "response_contract": {
            "output_schema": schema
        }
    });

    let contract = parse_response_contract(Some(&metadata))
        .expect("parse")
        .expect("should produce contract");

    let result = minimal_result(r#"{"result": "ok"}"#);
    let violations = validate_spawn_response(&result, &contract, None);
    assert!(violations.is_empty(), "{:?}", violations);

    let bad_result = minimal_result(r#"{"wrong": true}"#);
    let violations = validate_spawn_response(&bad_result, &contract, None);
    assert!(!violations.is_empty(), "should violate missing 'result'");
}

#[test]
fn egress_rejects_no_reply_when_schema_constrained() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["status"],
        "properties": {
            "status": { "type": "string" }
        }
    });
    let contract = contract_with_output_schema(schema);

    let mut result = minimal_result("");
    result.assistant_reply = None;

    let violations = validate_spawn_response(&result, &contract, None);
    assert!(
        !violations.is_empty(),
        "no reply should violate constrained schema"
    );
    assert!(
        violations
            .iter()
            .any(|v| v.message.contains("no reply produced")),
        "{:?}",
        violations
    );
}

#[test]
fn egress_enum_violation() {
    let schema = serde_json::json!({
        "type": "object",
        "required": ["status"],
        "properties": {
            "status": { "type": "string", "enum": ["ok", "error"] }
        }
    });
    let contract = contract_with_output_schema(schema);
    let result = minimal_result(r#"{"status": "unknown"}"#);

    let violations = validate_spawn_response(&result, &contract, None);
    assert!(!violations.is_empty(), "enum violation should be caught");
    assert!(
        violations.iter().any(|v| v.message.contains("not in enum")),
        "{:?}",
        violations
    );
}
