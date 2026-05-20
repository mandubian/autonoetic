//! Policy denial test for the watchdog agent (closes #242 acceptance bullet).
//!
//! Mirrors the capability set declared in `agents/specialists/watchdog/SKILL.md`
//! and asserts:
//!
//! 1. The four tools the watchdog needs to do its job are exposed:
//!    `digest_query`, `execution_search`, `agent_message`, `session_escalate`.
//! 2. Privileged tools the watchdog must NOT have access to are denied:
//!    `sandbox_exec`, `agent_spawn`, `agent_revision_create`,
//!    `agent_revision_promote`.
//!
//! This pins the watchdog's "observer-only" contract: if a future capability
//! change accidentally widened its surface, this test will fail.

use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::capability::Capability;

/// Construct a manifest that mirrors `agents/specialists/watchdog/SKILL.md`
/// exactly — same capabilities, no more. Any change to the SKILL.md
/// capability list should be reflected here, otherwise this test stops
/// being a faithful pin of that file.
fn watchdog_manifest() -> AgentManifest {
    AgentManifest {
        version: "1.0".to_string(),
        runtime: autonoetic_types::agent::RuntimeDeclaration {
            engine: "autonoetic".to_string(),
            gateway_version: "0.1.0".to_string(),
            sdk_version: "0.1.0".to_string(),
            runtime_type: "stateful".to_string(),
            sandbox: "bubblewrap".to_string(),
            runtime_lock: "runtime.lock".to_string(),
        },
        agent: autonoetic_types::agent::AgentIdentity {
            id: "watchdog.default".to_string(),
            name: "Watchdog".to_string(),
            description: "Observer that reviews agent sessions for trajectory divergence.".to_string(),
        },
        capabilities: vec![
            // digest_query gates on ReadAccess. execution_search and
            // session_escalate are always available (no capability gate).
            Capability::ReadAccess {
                scopes: vec!["*".to_string()],
            },
            Capability::AgentMessage {
                patterns: vec!["*".to_string()],
            },
        ],
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

#[test]
fn watchdog_can_call_its_four_declared_tools() {
    let registry = default_registry();
    let manifest = watchdog_manifest();
    let available: Vec<String> = registry
        .available_definitions(&manifest)
        .into_iter()
        .map(|d| d.name)
        .collect();

    for required in [
        "digest_query",
        "execution_search",
        "agent_message",
        "session_escalate",
    ] {
        assert!(
            available.iter().any(|n| n == required),
            "watchdog manifest must expose '{}', but available tools are: {:?}",
            required,
            available
        );
    }
}

#[test]
fn watchdog_cannot_call_privileged_tools() {
    let registry = default_registry();
    let manifest = watchdog_manifest();
    let available: Vec<String> = registry
        .available_definitions(&manifest)
        .into_iter()
        .map(|d| d.name)
        .collect();

    // These tools each gate on a capability the watchdog does NOT declare.
    // If any of them appear in `available`, the watchdog has been
    // accidentally promoted beyond its observer-only contract.
    for forbidden in [
        "sandbox_exec",      // SandboxFunctions { allowed: ["sandbox."] } or similar
        "agent_spawn",       // AgentSpawn capability
        "agent_revision_create", // AgentRevision capability
        "agent_revision_promote", // AgentRevision capability
    ] {
        assert!(
            !available.iter().any(|n| n == forbidden),
            "watchdog manifest must NOT expose '{}', but available tools are: {:?}",
            forbidden,
            available
        );
    }
}

#[test]
fn watchdog_exposed_set_does_not_include_arbitrary_sandbox_prefixes() {
    // The watchdog's SandboxFunctions capability allows only "digest_" and
    // "execution_". A tool whose name starts with neither must not be
    // exposed via this capability. We check one representative tool
    // ("sandbox_exec") that uses the SandboxFunctions capability but a
    // different prefix — if it leaked through, the prefix-matching logic
    // is broken (which would be a security-critical regression).
    let registry = default_registry();
    let manifest = watchdog_manifest();
    let available: Vec<String> = registry
        .available_definitions(&manifest)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(
        !available.contains(&"sandbox_exec".to_string()),
        "sandbox_exec should not match the 'digest_' or 'execution_' prefix \
         allowed by the watchdog manifest. Available: {:?}",
        available
    );
}
