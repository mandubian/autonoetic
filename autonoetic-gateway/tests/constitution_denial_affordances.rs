//! Citizenship RFC Part A.1 (issue #769): every permission denial must carry
//! a machine-readable `available_actions` field alongside `enforced_rules`
//! (Ri-0.3 named rejection), so the agent finds its lawful next moves
//! (propose an amendment, delegate, or inspect itself) inside the denial
//! itself.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::failure_classification::decorate_tool_error;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::tool_error::{tagged::Tagged, ToolError};

fn minimal_manifest_with_caps(caps: Vec<Capability>) -> AgentManifest {
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
        capabilities: caps,
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

fn assert_rules_are_constitutional(label: &str, rules: &[String]) {
    assert!(
        !rules.is_empty(),
        "{} rejection must carry at least one enforced rule (Ri-0.3)",
        label
    );
    for rule in rules {
        let first = rule.chars().next().unwrap_or(' ');
        assert!(
            matches!(first, 'P' | 'R' | 'I' | '§'),
            "{}: enforced rule '{}' must be a constitutional ID (P-/Ri-/I-/§) (Ri-0.3)",
            label,
            rule
        );
    }
}

/// Builds a decorated permission-denial `ToolError` the way real tool call
/// sites do: `PolicyEngine` produces a denial `PolicyDecision`, its rule IDs
/// are attached via `Tagged::permission_with_rules`, converted to a
/// `ToolError`, then run through the same `decorate_tool_error` central
/// point used by `tool_call_processor`.
fn denial_tool_error() -> ToolError {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = PolicyEngine::new(manifest);
    let decision = policy.can_connect_net("api.example.com");
    assert!(!decision.is_allowed());

    let tagged = Tagged::permission_with_rules(
        anyhow::anyhow!("NetworkAccess required for host api.example.com"),
        decision.into_rule_ids(),
    );
    let err: ToolError = tagged.into();
    decorate_tool_error(err)
}

#[test]
fn permission_denial_carries_enforced_rules_and_available_actions() {
    let err = denial_tool_error();

    assert_rules_are_constitutional("NetworkAccess", &err.enforced_rules);

    assert!(
        !err.available_actions.is_empty(),
        "permission denial must carry available_actions"
    );

    let propose = err
        .available_actions
        .iter()
        .find(|a| a.action == "propose_amendment")
        .expect("propose_amendment action must be present");
    assert_eq!(
        propose.tool.as_deref(),
        Some("constitution_propose_amendment")
    );
    assert_eq!(propose.clause.as_deref(), Some("Ri-0.8"));
}

#[test]
fn permission_denial_json_includes_available_actions_key() {
    let err = denial_tool_error();
    let json = err.to_json_string();
    assert!(
        json.contains("available_actions"),
        "serialized denial must include available_actions: {json}"
    );
}
