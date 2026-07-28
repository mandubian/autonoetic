//! Constitution Phase 4.3/4.4 pin: any observed remote-access signal must be
//! covered by a manifest declaration; missing declaration fails shut.

use std::sync::Arc;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use tempfile::tempdir;

fn manifest(agent_id: &str, capabilities: Vec<Capability>) -> AgentManifest {
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
            description: "test agent".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
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

fn run_sandbox_exec(manifest: &AgentManifest, command: &str) -> anyhow::Result<serde_json::Value> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    let cfg = GatewayConfig {
        agents_dir,
        ..GatewayConfig::default()
    };

    let store = Arc::new(GatewayStore::open(temp.path())?);
    let registry = default_registry();
    let policy = PolicyEngine::new(manifest.clone());
    let out = registry.execute(
        "sandbox_exec",
        manifest,
        &policy,
        temp.path(),
        Some(temp.path()),
        &json!({ "command": command }).to_string(),
        Some("root-1/session-1"),
        None,
        Some(&cfg),
        Some(store),
        None,
    )?;
    Ok(serde_json::from_str(&out)?)
}

#[test]
fn network_access_agent_remote_signal_without_declaration_fails_shut() -> anyhow::Result<()> {
    let manifest = manifest(
        "strict-net.default",
        vec![
            Capability::CodeExecution {
                patterns: vec!["*".to_string()],
                commands: vec![],
            },
            Capability::NetworkAccess {
                hosts: vec!["*".to_string()],
            },
        ],
    );

    let body = run_sandbox_exec(&manifest, "curl https://api.example.com/v1")?;
    assert_eq!(body["ok"], false);
    assert_eq!(body["error_type"], "missing_remote_access_declaration");
    let msg = body["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("has no metadata.autonoetic.remote_access declaration"),
        "unexpected message: {msg}"
    );
    Ok(())
}

#[test]
fn non_network_agent_remote_signal_without_declaration_fails_shut() -> anyhow::Result<()> {
    let manifest = manifest(
        "no-net.remote.default",
        vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
    );

    let body = run_sandbox_exec(&manifest, "curl https://api.example.com/v1")?;
    assert_eq!(body["ok"], false);
    assert_eq!(body["error_type"], "missing_remote_access_declaration");
    Ok(())
}

#[test]
fn non_network_agent_without_remote_signal_is_not_blocked_by_declaration_gate() -> anyhow::Result<()>
{
    let manifest = manifest(
        "no-net.default",
        vec![Capability::CodeExecution {
            patterns: vec!["*".to_string()],
            commands: vec![],
        }],
    );

    let body = run_sandbox_exec(&manifest, "echo declaration-gate-ok")?;
    assert_ne!(body["error_type"], "missing_remote_access_declaration");
    Ok(())
}
