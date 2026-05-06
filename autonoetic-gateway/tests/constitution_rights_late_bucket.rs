//! Constitution §2.16: Rights audit late bucket.
//!
//! Rights whose enforcement mechanism is already in place from other
//! Phase 1/2 work.

mod support;

use autonoetic_gateway::runtime::tools::{default_registry, ToolTierFilter};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;

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
        allowed_tool_tiers: vec![],
        agentskills_import: None,
        compression: None,
    }
}

// ---------------------------------------------------------------------------
// Ri-0.1: Self-inspection
// ---------------------------------------------------------------------------

#[test]
fn ri_0_1_registry_provides_inspection_tools_when_capable() {
    let manifest = minimal_manifest_with_caps(vec![Capability::AgentRevision {
        patterns: vec!["*".to_string()],
    }]);
    let registry = default_registry();
    let defs = registry.available_definitions(&manifest);
    let tool_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        tool_names.contains(&"agent_revision_inspect"),
        "agent_revision_inspect must be available for Ri-0.1 self-inspection: {:?}",
        tool_names
    );
    assert!(
        tool_names.contains(&"agent_revision_list"),
        "agent_revision_list must be available for Ri-0.1 self-inspection"
    );
}

#[test]
fn ri_0_1_registry_provides_knowledge_tools_for_memory_inspection() {
    let manifest = minimal_manifest_with_caps(vec![Capability::ReadAccess {
        scopes: vec!["self.*".to_string()],
    }]);
    let registry = default_registry();
    let defs = registry.available_definitions(&manifest);
    let tool_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        tool_names.contains(&"knowledge_recall"),
        "knowledge_recall must be available for memory self-inspection"
    );
}

// ---------------------------------------------------------------------------
// Ri-0.3: Named rejection reason (rejections carry rule IDs)
// ---------------------------------------------------------------------------

fn assert_rules_are_constitutional(label: &str, rules: &[&'static str]) {
    assert!(
        !rules.is_empty(),
        "{} rejection must carry at least one enforced rule (Ri-0.3)",
        label
    );
    for rule in rules {
        let s = rule.to_string();
        assert!(
            s.starts_with('R') || s.starts_with('§'),
            "{}: enforced rule '{}' must start with R or § (Ri-0.3)",
            label, s
        );
    }
}

#[test]
fn ri_0_3_capability_rejection_carries_rule_ids() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest);
    let decision = policy.can_agent_revision("other.agent");
    assert!(!decision.is_allowed());
    assert_rules_are_constitutional("AgentRevision", &decision.enforced_rules);
}

#[test]
fn ri_0_3_network_rejection_carries_rule_ids() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest);
    let decision = policy.can_connect_net("api.example.com");
    assert!(!decision.is_allowed());
    assert_rules_are_constitutional("NetworkAccess", &decision.enforced_rules);
}

#[test]
fn ri_0_3_exec_rejection_carries_rule_ids() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest);
    let decision = policy.can_exec_shell("ls");
    assert!(!decision.is_allowed());
    assert_rules_are_constitutional("CodeExecution", &decision.enforced_rules);
}

#[test]
fn ri_0_3_spawn_rejection_carries_rule_ids() {
    let manifest = minimal_manifest_with_caps(vec![]);
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest);
    let decision = policy.can_spawn_agent();
    assert!(!decision.is_allowed());
    assert_rules_are_constitutional("AgentSpawn", &decision.enforced_rules);
}

#[test]
fn ri_0_3_allowed_operations_are_allowed() {
    let manifest = minimal_manifest_with_caps(vec![Capability::NetworkAccess {
        hosts: vec!["api.example.com".to_string()],
    }]);
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest);
    let decision = policy.can_connect_net("api.example.com");
    assert!(decision.is_allowed());
}

// ---------------------------------------------------------------------------
// Ri-0.12 cross-check: tool tier enforcement is consistent
// ---------------------------------------------------------------------------

#[test]
fn ri_0_12_core_tools_available_in_degraded_mode() {
    let core = ToolTierFilter::core_only();
    assert!(core.allows("content_read"));
    assert!(core.allows("content_write"));
    assert!(core.allows("knowledge_recall"));
    assert!(core.allows("knowledge_store"));

    let all = ToolTierFilter::all();
    assert!(all.allows("content_read"));
    assert!(all.allows("web_search"));
}
