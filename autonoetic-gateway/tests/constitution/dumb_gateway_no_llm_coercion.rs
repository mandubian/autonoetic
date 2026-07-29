//! Constitution Phase 4.2: schema enforcement must not use gateway-internal LLM coercion.
//!
//! Pin: `schema_enforcement.mode: llm` is rejected at config parse time.

use autonoetic_types::config::GatewayConfig;

#[test]
fn rejects_legacy_llm_schema_enforcement_mode() {
    let yaml = r#"
agents_dir: "/tmp/autonoetic-agents"
schema_enforcement:
  mode: llm
  audit: true
"#;

    let err = serde_yaml::from_str::<GatewayConfig>(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown variant") && msg.contains("llm"),
        "expected unknown-variant rejection for schema_enforcement.mode=llm, got: {}",
        msg
    );
}

#[test]
fn rejects_legacy_llm_schema_enforcement_override_mode() {
    let yaml = r#"
agents_dir: "/tmp/autonoetic-agents"
schema_enforcement:
  mode: deterministic
  audit: true
  agent_overrides:
    planner.default: llm
"#;

    let err = serde_yaml::from_str::<GatewayConfig>(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown variant") && msg.contains("llm"),
        "expected unknown-variant rejection for schema_enforcement.agent_overrides.*=llm, got: {}",
        msg
    );
}

#[test]
fn accepts_deterministic_schema_enforcement_mode() {
    let yaml = r#"
agents_dir: "/tmp/autonoetic-agents"
schema_enforcement:
  mode: deterministic
  audit: true
"#;

    let parsed =
        serde_yaml::from_str::<GatewayConfig>(yaml).expect("deterministic mode should parse");
    assert_eq!(
        parsed.schema_enforcement.mode,
        autonoetic_types::config::SchemaEnforcementMode::Deterministic
    );
}
