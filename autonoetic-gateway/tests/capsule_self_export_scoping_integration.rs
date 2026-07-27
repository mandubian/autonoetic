//! Ri-0.17 enforcement: an agent holding `SelfCapsuleExport` may export only
//! its *own* cognitive capsule (`agent_id == manifest.agent.id`), and no other.
//! The broad `CapsuleExport` (operator-granted) remains the path for exporting
//! any agent. Mirrors the target-scoped capability pattern in
//! `constitution_private_reasoning_c.rs`.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::capsule::{CapsuleExportTool, CapsuleImportTool};
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::agent::SandboxNetworkPolicy;

fn manifest_with(agent_id: &str, caps: Vec<Capability>) -> AgentManifest {
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
            id: agent_id.to_string(),
            name: agent_id.to_string(),
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
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: SandboxNetworkPolicy::default(),
        excluded_tools: vec![],
    }
}

/// `SelfCapsuleExport` holders may export their own agent_id.
#[test]
fn self_export_allowed_for_own_agent_id() {
    let manifest = manifest_with("alpha", vec![Capability::SelfCapsuleExport]);
    let policy = PolicyEngine::new(manifest);
    let decision = policy.can_use_capsule_self("alpha");
    assert!(
        decision.is_allowed(),
        "an agent with SelfCapsuleExport must be allowed to export its own agent_id"
    );
    assert!(
        decision.enforced_rules.iter().any(|r| *r == "Ri-0.17"),
        "the allow decision must carry rule Ri-0.17"
    );
}

/// `SelfCapsuleExport` holders must NOT export another agent's capsule.
#[test]
fn self_export_denied_for_other_agent_id() {
    let manifest = manifest_with("alpha", vec![Capability::SelfCapsuleExport]);
    let policy = PolicyEngine::new(manifest);
    let decision = policy.can_use_capsule_self("beta");
    assert!(
        !decision.is_allowed(),
        "an agent with SelfCapsuleExport must be denied export of a different agent_id"
    );
    assert!(
        decision.enforced_rules.iter().any(|r| *r == "Ri-0.17"),
        "the deny decision must carry rule Ri-0.17"
    );
}

/// Without any capsule capability, self-export is denied.
#[test]
fn self_export_denied_without_capability() {
    let manifest = manifest_with(
        "alpha",
        vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
    );
    let policy = PolicyEngine::new(manifest);
    assert!(
        !policy.can_use_capsule_self("alpha").is_allowed(),
        "no capsule capability must deny self-export even of own agent_id"
    );
}

/// A broad `CapsuleExport` holder is *not* implicitly granted self-scoped
/// export via `can_use_capsule_self` — the broad path is a separate gate
/// (`can_use_capsule`) that the tool checks first. This guards the two-tier
/// separation: broad holders must not also satisfy the scoped check, otherwise
/// the scoped check would become redundant and the rule-id attribution wrong.
#[test]
fn broad_capsule_export_does_not_satisfy_self_scoped_check() {
    let manifest = manifest_with("alpha", vec![Capability::CapsuleExport]);
    let policy = PolicyEngine::new(manifest);
    // The broad capability gates export via can_use_capsule (tested elsewhere);
    // can_use_capsule_self is the scoped fallback only.
    assert!(
        !policy.can_use_capsule_self("alpha").is_allowed(),
        "CapsuleExport must not satisfy the SelfCapsuleExport-scoped check"
    );
    assert!(
        policy.can_use_capsule().is_allowed(),
        "CapsuleExport must still gate the broad path"
    );
}

/// The export tool is visible (`is_available`) when the agent holds only
/// `SelfCapsuleExport`, so the scoped agent can attempt (and be scoped on)
/// self-export.
#[test]
fn tool_available_with_self_capsule_export_capability() {
    let manifest = manifest_with("alpha", vec![Capability::SelfCapsuleExport]);
    assert!(
        CapsuleExportTool.is_available(&manifest),
        "capsule_export must be available with SelfCapsuleExport"
    );
    // Import is gated by the broad capability only — a self-only holder must
    // NOT see capsule_import.
    assert!(
        !CapsuleImportTool.is_available(&manifest),
        "capsule_import must not be available with only SelfCapsuleExport"
    );
}

/// Neither capability → tool not available.
#[test]
fn tool_unavailable_without_any_capsule_capability() {
    let manifest = manifest_with(
        "alpha",
        vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
    );
    assert!(!CapsuleExportTool.is_available(&manifest));
}
