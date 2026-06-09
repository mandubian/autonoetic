//! Constitution Ri-0.10 — Right to read the constitution under which the
//! agent is operating. Issue #95.
//!
//! Verifies the `constitution_read` native tool:
//!   - is available to every agent regardless of declared capabilities
//!   - returns the full document with no args
//!   - supports section selectors for rule IDs and §N forms
//!   - returns a SHA-256 digest matching the canonical digest payload
//!   - rejects unknown selectors with a structured validation error.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use sha2::{Digest, Sha256};
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
        // The whole point of Ri-0.10: NO capabilities required to read the law.
        capabilities: vec![],
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

fn invoke(args_json: &str) -> serde_json::Value {
    let temp = tempdir().expect("tempdir");
    let manifest = no_capability_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();
    let raw = registry
        .execute(
            "constitution_read",
            &manifest,
            &policy,
            temp.path(),
            None,
            args_json,
            None,
            None,
            Some(&gateway_config),
            None,
            None,
        )
        .expect("constitution_read execute should not error");
    serde_json::from_str(&raw).expect("response is JSON")
}

#[test]
fn tool_is_registered_and_available_without_capabilities() {
    let registry = default_registry();
    assert!(
        registry.has_tool("constitution_read"),
        "constitution_read must be registered in default_registry"
    );

    let manifest = no_capability_manifest();
    let defs = registry.available_definitions(&manifest);
    assert!(
        defs.iter().any(|d| d.name == "constitution_read"),
        "constitution_read must be available to every agent (Ri-0.10)"
    );
}

#[test]
fn empty_args_returns_full_constitution() {
    let resp = invoke("{}");
    assert_eq!(resp["ok"], true);
    let text = resp["text"].as_str().expect("text is a string");

    // Full document covers from the title through the closing sections.
    assert!(
        text.starts_with("# Gateway Constitution"),
        "starts with title"
    );
    assert!(
        text.contains("## 0. Bill of Rights"),
        "contains Bill of Rights section"
    );
    assert!(
        text.contains("## 14."),
        "contains Lawful-Executor invariant section"
    );
    assert!(text.contains("Ri-0.10"), "contains Ri-0.10 row");

    assert!(resp["digest"].is_string(), "digest present");
    assert!(resp["version"].is_string(), "version present");
    assert!(resp["retrieved_at"].is_string(), "retrieved_at present");
    assert!(
        resp["section"].is_null(),
        "section is null when no selector"
    );
}

#[test]
fn empty_string_args_also_returns_full_constitution() {
    // Some LLMs send "" rather than "{}" when calling no-arg tools.
    let resp = invoke("");
    assert_eq!(resp["ok"], true);
    let text = resp["text"].as_str().expect("text is a string");
    assert!(text.contains("Ri-0.10"));
}

#[test]
fn section_selector_ri_0_10_returns_scoped_row() {
    let resp = invoke(r#"{"section":"Ri-0.10"}"#);
    assert_eq!(resp["ok"], true);
    let text = resp["text"].as_str().expect("text");
    assert!(text.contains("Ri-0.10"), "scoped text contains the rule ID");
    assert!(
        !text.contains("## 14."),
        "scoped text does not contain unrelated section 14: {}",
        text
    );
    assert_eq!(resp["section"], "Ri-0.10");
}

#[test]
fn section_selector_section_zero_returns_rights_section() {
    let resp = invoke(r#"{"section":"§0"}"#);
    assert_eq!(resp["ok"], true);
    let text = resp["text"].as_str().expect("text");
    assert!(text.starts_with("## 0. "), "starts with rights heading");
    assert!(text.contains("Ri-0.10"));
    assert!(
        !text.contains("\n## 1. "),
        "scoped text does not bleed into section 1"
    );
    assert_eq!(resp["section"], "§0");
}

#[test]
fn section_selector_pending_rule() {
    let resp = invoke(r#"{"section":"R+++3"}"#);
    assert_eq!(resp["ok"], true);
    let text = resp["text"].as_str().expect("text");
    assert!(text.contains("R+++3"));
}

#[test]
fn unknown_selector_returns_validation_error() {
    let resp = invoke(r#"{"section":"Ri-9.99"}"#);
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error_type"], "validation");
    let msg = resp["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("Ri-9.99"),
        "error message echoes the bad selector: {}",
        msg
    );
}

#[test]
fn digest_matches_sha256_of_source_markdown() {
    // The digest returned by the tool must equal the canonical digest payload
    // used for federation compatibility checks.
    autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    )
    .expect("default constitution should initialize");
    let payload = serde_json::json!({
        "constitution_text": autonoetic_gateway::constitution_digest::constitution_text().as_ref(),
        "rights_enforcement": autonoetic_gateway::constitution_digest::canonical_right_enforcement_table(),
        "rules_enforcement": autonoetic_gateway::constitution_digest::canonical_rule_enforcement_table(),
    });
    let payload_bytes =
        serde_json::to_vec(&payload).expect("canonical constitution payload should serialize");
    let mut hasher = Sha256::new();
    hasher.update(payload_bytes);
    let expected = hex::encode(hasher.finalize());

    let resp = invoke("{}");
    assert_eq!(
        resp["digest"].as_str().expect("digest"),
        expected,
        "tool digest must match canonical constitution digest payload"
    );
}

#[test]
fn invalid_json_args_returns_anyhow_error() {
    // Distinct from the validation-error envelope: malformed JSON arguments
    // are a tool invocation contract violation, not a section selector miss.
    let temp = tempdir().expect("tempdir");
    let manifest = no_capability_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let gateway_config = autonoetic_types::config::GatewayConfig::default();
    let result = registry.execute(
        "constitution_read",
        &manifest,
        &policy,
        temp.path(),
        None,
        "not json",
        None,
        None,
        Some(&gateway_config),
        None,
        None,
    );
    assert!(result.is_err(), "malformed JSON must error");
}
