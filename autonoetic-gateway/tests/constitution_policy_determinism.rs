//! Constitution R++9: Policy engine determinism property test.
//!
//! Verifies that the gateway's policy decisions are pure functions of
//! their declared inputs (capability-set, tool-call). Every PolicyEngine
//! method is exercised with a fixed matrix of inputs covering allowed
//! and denied paths; the same inputs always produce the same verdict
//! with identical enforced_rules and security_analysis. No LLM call,
//! no network fetch, no hidden branch, no time-dependent or random state.
//!
//! Any future change that adds nondeterminism to PolicyEngine will
//! fail these tests.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;

fn base_manifest(caps: Vec<Capability>) -> AgentManifest {
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
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn network_manifest(hosts: Vec<&str>) -> AgentManifest {
    base_manifest(vec![Capability::NetworkAccess {
        hosts: hosts.into_iter().map(String::from).collect(),
    }])
}

fn code_exec_manifest(patterns: Vec<&str>) -> AgentManifest {
    base_manifest(vec![Capability::CodeExecution {
        patterns: patterns.into_iter().map(String::from).collect(),
        commands: vec![],
    }])
}

fn sandbox_fn_manifest(allowed: Vec<&str>) -> AgentManifest {
    base_manifest(vec![Capability::SandboxFunctions {
        allowed: allowed.into_iter().map(String::from).collect(),
    }])
}

fn read_access_manifest(scopes: Vec<&str>) -> AgentManifest {
    base_manifest(vec![Capability::ReadAccess {
        scopes: scopes.into_iter().map(String::from).collect(),
    }])
}

fn write_access_manifest(scopes: Vec<&str>) -> AgentManifest {
    base_manifest(vec![Capability::WriteAccess {
        scopes: scopes.into_iter().map(String::from).collect(),
    }])
}

fn spawn_manifest(max_children: u32) -> AgentManifest {
    base_manifest(vec![Capability::AgentSpawn {
        max_children,
        max_spawn_depth: 0,
    }])
}

fn message_manifest(patterns: Vec<&str>) -> AgentManifest {
    base_manifest(vec![Capability::AgentMessage {
        patterns: patterns.into_iter().map(String::from).collect(),
    }])
}

fn assert_decision_eq(
    a: &autonoetic_gateway::policy::PolicyDecision,
    b: &autonoetic_gateway::policy::PolicyDecision,
    context: &str,
) {
    assert_eq!(
        a.is_allowed(),
        b.is_allowed(),
        "{}: allow/deny mismatch",
        context
    );
    assert_eq!(
        a.enforced_rules, b.enforced_rules,
        "{}: enforced_rules mismatch",
        context
    );
    assert_eq!(
        a.security_analysis.as_ref().map(|s| &s.threats),
        b.security_analysis.as_ref().map(|s| &s.threats),
        "{}: security_analysis threats mismatch",
        context
    );
}

// --- Idempotency: same inputs → same verdict ---

#[test]
fn r_plus_plus_9_can_connect_net_idempotent() {
    let engine = PolicyEngine::new(network_manifest(vec!["api.example.com", "*.cdn.org"]));
    for host in &["api.example.com", "cdn.org", "unknown.com", "*", ""] {
        let a = engine.can_connect_net(host);
        let b = engine.can_connect_net(host);
        assert_decision_eq(&a, &b, &format!("can_connect_net({})", host));
    }
}

#[test]
fn r_plus_plus_9_can_exec_shell_idempotent() {
    let engine = PolicyEngine::new(code_exec_manifest(vec!["python3", "bash"]));
    for cmd in &[
        "python3 script.py",
        "bash -c 'echo hi'",
        "rm -rf /",
        "curl evil.com",
        "",
    ] {
        let a = engine.can_exec_shell(cmd);
        let b = engine.can_exec_shell(cmd);
        assert_decision_eq(&a, &b, &format!("can_exec_shell({})", cmd));
    }
}

#[test]
fn r_plus_plus_9_can_invoke_tool_idempotent() {
    let engine = PolicyEngine::new(sandbox_fn_manifest(vec!["web.", "sandbox.exec"]));
    for tool in &["web.fetch", "sandbox.exec", "unknown.tool", ""] {
        let a = engine.can_invoke_tool(tool);
        let b = engine.can_invoke_tool(tool);
        assert_decision_eq(&a, &b, &format!("can_invoke_tool({})", tool));
    }
}

#[test]
fn r_plus_plus_9_can_read_path_idempotent() {
    let engine = PolicyEngine::new(read_access_manifest(vec!["self/", "shared/"]));
    for path in &["self/data", "shared/file.txt", "other/secret", ""] {
        let a = engine.can_read_path(path);
        let b = engine.can_read_path(path);
        assert_decision_eq(&a, &b, &format!("can_read_path({})", path));
    }
}

#[test]
fn r_plus_plus_9_can_write_path_idempotent() {
    let engine = PolicyEngine::new(write_access_manifest(vec!["self/"]));
    for path in &["self/out", "etc/passwd", ""] {
        let a = engine.can_write_path(path);
        let b = engine.can_write_path(path);
        assert_decision_eq(&a, &b, &format!("can_write_path({})", path));
    }
}

#[test]
fn r_plus_plus_9_can_spawn_agent_idempotent() {
    let engine = PolicyEngine::new(spawn_manifest(5));
    let a = engine.can_spawn_agent();
    let b = engine.can_spawn_agent();
    assert_eq!(a.is_allowed(), b.is_allowed());

    let no_spawn = PolicyEngine::new(base_manifest(vec![]));
    let c = no_spawn.can_spawn_agent();
    let d = no_spawn.can_spawn_agent();
    assert_eq!(c.is_allowed(), d.is_allowed());
}

#[test]
fn r_plus_plus_9_can_message_agent_idempotent() {
    let engine = PolicyEngine::new(message_manifest(vec!["coder.*"]));
    for target in &["coder.default", "planner.default", ""] {
        let a = engine.can_message_agent(target);
        let b = engine.can_message_agent(target);
        assert_decision_eq(&a, &b, &format!("can_message_agent({})", target));
    }
}

// --- Input sensitivity: different inputs → different verdicts ---

#[test]
fn r_plus_plus_9_network_verdict_depends_on_host() {
    let engine = PolicyEngine::new(network_manifest(vec!["api.example.com"]));
    assert!(engine.can_connect_net("api.example.com").is_allowed());
    assert!(!engine.can_connect_net("evil.com").is_allowed());
}

#[test]
fn r_plus_plus_9_shell_verdict_depends_on_command() {
    let engine = PolicyEngine::new(code_exec_manifest(vec!["python3"]));
    assert!(engine.can_exec_shell("python3 run.py").is_allowed());
    assert!(!engine.can_exec_shell("rm -rf /").is_allowed());
}

#[test]
fn r_plus_plus_9_tool_verdict_depends_on_name() {
    let engine = PolicyEngine::new(sandbox_fn_manifest(vec!["web."]));
    assert!(engine.can_invoke_tool("web.fetch").is_allowed());
    assert!(!engine.can_invoke_tool("sandbox.exec").is_allowed());
}

#[test]
fn r_plus_plus_9_no_caps_denies_everything() {
    let engine = PolicyEngine::new(base_manifest(vec![]));
    assert!(!engine.can_connect_net("any.com").is_allowed());
    assert!(!engine.can_exec_shell("echo hi").is_allowed());
    assert!(!engine.can_invoke_tool("any.tool").is_allowed());
    assert!(!engine.can_read_path("any/path").is_allowed());
    assert!(!engine.can_write_path("any/path").is_allowed());
    assert!(!engine.can_spawn_agent().is_allowed());
    assert!(!engine.can_message_agent("any.agent").is_allowed());
    assert!(!engine.can_request_emergency_stop().is_allowed());
}

#[test]
fn r_plus_plus_9_wildcard_allows_all_hosts() {
    let engine = PolicyEngine::new(network_manifest(vec!["*"]));
    assert!(engine.can_connect_net("anything.example.com").is_allowed());
    assert!(engine.can_connect_net("192.168.1.1").is_allowed());
}

#[test]
fn r_plus_plus_9_verdict_is_pure_over_many_calls() {
    let engine = PolicyEngine::new(base_manifest(vec![
        Capability::NetworkAccess {
            hosts: vec!["api.github.com".to_string()],
        },
        Capability::CodeExecution {
            patterns: vec!["cargo".to_string()],
            commands: vec![],
        },
        Capability::SandboxFunctions {
            allowed: vec!["web.".to_string()],
        },
    ]));

    for _ in 0..100 {
        assert!(engine.can_connect_net("api.github.com").is_allowed());
        assert!(!engine.can_connect_net("other.com").is_allowed());
        assert!(engine.can_exec_shell("cargo test").is_allowed());
        assert!(!engine.can_exec_shell("rm -rf /").is_allowed());
        assert!(engine.can_invoke_tool("web.search").is_allowed());
        assert!(!engine.can_invoke_tool("admin.wipe").is_allowed());
    }
}

#[test]
fn r_plus_plus_9_emergency_stop_requires_capability() {
    let with_cap = base_manifest(vec![Capability::EmergencyStop]);
    let without_cap = base_manifest(vec![]);
    assert!(PolicyEngine::new(with_cap)
        .can_request_emergency_stop()
        .is_allowed());
    assert!(!PolicyEngine::new(without_cap)
        .can_request_emergency_stop()
        .is_allowed());
}

#[test]
fn r_plus_plus_9_spawn_limit_reflects_capability() {
    let engine = PolicyEngine::new(spawn_manifest(10));
    assert_eq!(engine.spawn_agent_limit(), Some(10));

    let no_spawn = PolicyEngine::new(base_manifest(vec![]));
    assert_eq!(no_spawn.spawn_agent_limit(), None);
}
