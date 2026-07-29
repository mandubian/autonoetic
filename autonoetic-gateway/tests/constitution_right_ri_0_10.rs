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
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use support::manifest_builder::TestManifest;

fn no_capability_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        // The whole point of Ri-0.10: NO capabilities required to read the law.
        ..TestManifest::new().build()
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
fn section_selector_single_rule_row() {
    // Select one rule by its identifier (the `extract_rule_row` path). Uses a
    // live, stable rule; the old "R+++N" pending-rule notation was promoted to
    // numbered rule rows and no longer exists in the active constitution.
    let resp = invoke(r#"{"section":"P-5.11"}"#);
    assert_eq!(resp["ok"], true);
    let text = resp["text"].as_str().expect("text");
    assert!(text.contains("P-5.11"));
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
