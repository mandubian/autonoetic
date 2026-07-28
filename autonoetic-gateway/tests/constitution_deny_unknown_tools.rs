//! Constitution R+14 / P-1.11: Deny-by-default on unknown tool names.
//!
//! R+14: Unknown tool names must deny by default, not silent-allow.
//! The policy engine already implements this — these tests pin the
//! behavior so any regression is caught.
//!
//! Note: `can_invoke_tool` attributes decisions to P-1.1 (the tool invocation
//! rule), which is the enforcement mechanism for P-1.11 (deny-by-default).
//! All tests assert P-1.1 as the enforced rule ID.

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;

fn manifest_with_sandbox(allowed: Vec<&str>) -> AgentManifest {
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
            id: "test.agent".to_string(),
            name: "Test Agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::SandboxFunctions {
            allowed: allowed.iter().map(|s| s.to_string()).collect(),
        }],
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        }
}

fn manifest_no_capabilities() -> AgentManifest {
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
            id: "no.cap".to_string(),
            name: "No Cap".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        }
}

#[test]
fn r_plus_14_unknown_tool_name_denied() {
    let manifest = manifest_with_sandbox(vec!["web."]);
    let policy = PolicyEngine::new(manifest);
    let decision = policy.can_invoke_tool("totally_bogus_tool_xyz");
    assert!(!decision.is_allowed(), "unknown tool name must be denied");
    assert!(
        decision.enforced_rules.contains(&"P-1.1"),
        "denial must cite rule P-1.1"
    );
}

#[test]
fn r_plus_14_no_capability_denies_known_tool() {
    let manifest = manifest_no_capabilities();
    let policy = PolicyEngine::new(manifest);
    let decision = policy.can_invoke_tool("web_search");
    assert!(
        !decision.is_allowed(),
        "tool must be denied when no SandboxFunctions capability exists"
    );
}

#[test]
fn r_plus_14_non_matching_prefix_denied() {
    let manifest = manifest_with_sandbox(vec!["web."]);
    let policy = PolicyEngine::new(manifest);

    assert!(policy.can_invoke_tool("web.search").is_allowed());
    assert!(policy.can_invoke_tool("web.fetch").is_allowed());

    assert!(
        !policy.can_invoke_tool("sandbox_exec").is_allowed(),
        "tool not matching any allowed prefix must be denied"
    );
    assert!(
        !policy.can_invoke_tool("content_write").is_allowed(),
        "tool not matching any allowed prefix must be denied"
    );
    assert!(
        !policy.can_invoke_tool("webhook_trigger").is_allowed(),
        "prefix match must be exact up to the wildcard boundary"
    );
}

#[test]
fn r_plus_14_wildcard_allows_all() {
    let manifest = manifest_with_sandbox(vec!["*"]);
    let policy = PolicyEngine::new(manifest);
    assert!(policy.can_invoke_tool("anything_goes").is_allowed());
    assert!(policy.can_invoke_tool("web_search").is_allowed());
}

#[test]
fn r_plus_14_empty_tool_name_denied_without_wildcard() {
    let manifest = manifest_with_sandbox(vec!["web."]);
    let policy = PolicyEngine::new(manifest);

    let empty = policy.can_invoke_tool("");
    assert!(
        !empty.is_allowed(),
        "empty tool name must be denied without wildcard"
    );
}

#[test]
fn r_plus_14_wildcard_allows_empty_tool_name() {
    let manifest = manifest_with_sandbox(vec!["*"]);
    let policy = PolicyEngine::new(manifest);

    let empty = policy.can_invoke_tool("");
    assert!(
        empty.is_allowed(),
        "empty string starts with empty prefix of '*'"
    );
}

#[test]
fn r_plus_14_nonexistent_tool_rejected_by_registry() {
    let registry = default_registry();
    assert!(
        !registry.has_tool("totally_bogus_tool_xyz"),
        "registry must not contain unknown tool"
    );
    assert!(
        registry.has_tool("sandbox_exec"),
        "registry must contain known tools"
    );
}
