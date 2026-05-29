//! Constitution test: R+3 / P-7.15 — Spawn-chain depth cap.
//!
//! Verifies that agents whose session depth equals or exceeds the configured
//! ceiling are refused the right to spawn further children. The effective
//! ceiling is `min(agent_max_spawn_depth, system_max_spawn_depth)` where
//! `agent_max_spawn_depth == 0` means "use the system default".

mod support;

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;

fn install_spawn_agent(
    agents_dir: &std::path::Path,
    name: &str,
    max_children: u32,
    max_spawn_depth: u32,
) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join(name);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        format!(
            r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{name}"
  name: "{name}"
  description: "Test spawn agent"
capabilities:
  - type: "AgentSpawn"
    max_children: {max_children}
    max_spawn_depth: {max_spawn_depth}
  - type: "ReadAccess"
    scopes: ["*"]
  - type: "WriteAccess"
    scopes: ["*"]
---
# {name}
Do nothing.
"#
        ),
    )?;
    Ok(())
}

fn install_target_agent(agents_dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
    let agent_dir = agents_dir.join(name);
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []")?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        format!(
            r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "{name}"
  name: "{name}"
  description: "Target leaf agent"
capabilities: []
---
# {name}
Reply with "ok".
"#
        ),
    )?;
    Ok(())
}

fn seed_revision(
    store: &autonoetic_gateway::scheduler::gateway_store::GatewayStore,
    config: &GatewayConfig,
    agent_id: &str,
    agents_dir: &std::path::Path,
) -> anyhow::Result<String> {
    support::seed_agent_revision(store, config, agent_id, &agents_dir.join(agent_id))
}

/// Build a GatewayExecutionService with a low max_spawn_depth for testing.
fn make_config(agents_dir: &std::path::Path, max_spawn_depth: u32) -> GatewayConfig {
    let mut config = GatewayConfig {
        agents_dir: agents_dir.to_path_buf(),
        max_spawn_depth,
        ..GatewayConfig::default()
    };
    config.agents_dir = agents_dir.to_path_buf();
    config
}

#[serial_test::serial]
#[tokio::test]
async fn spawn_refused_at_system_ceiling() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = make_config(&workspace.agents_dir, 2);
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    install_spawn_agent(&workspace.agents_dir, "spawner", 10, 0)?;
    install_target_agent(&workspace.agents_dir, "leaf")?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    seed_revision(&store, &config, "spawner", &workspace.agents_dir)?;
    seed_revision(&store, &config, "leaf", &workspace.agents_dir)?;

    let exec = Arc::new(GatewayExecutionService::new(config.clone(), Some(store)));

    // Session "root" → depth 0: should be allowed (depth 0 < ceiling 2)
    let result: Result<autonoetic_gateway::SpawnResult, anyhow::Error> = exec
        .spawn_agent_once(
            "leaf",
            "hello",
            "root",
            Some("spawner"),
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await;
    // May fail for other reasons (no LLM stub), but should NOT fail with depth error
    match &result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("spawn-chain depth"),
                "depth cap should not trigger at depth 0, but got: {}",
                msg
            );
        }
        Ok(_) => {}
    }

    // Session "root/child-abc" → depth 1: should be allowed (depth 1 < ceiling 2)
    let result: Result<autonoetic_gateway::SpawnResult, anyhow::Error> = exec
        .spawn_agent_once(
            "leaf",
            "hello",
            "root/child-abc",
            Some("spawner"),
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await;
    match &result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("spawn-chain depth"),
                "depth cap should not trigger at depth 1, but got: {}",
                msg
            );
        }
        Ok(_) => {}
    }

    // Session "root/child-abc/grandchild-def" → depth 2: should be REFUSED (depth 2 >= ceiling 2)
    let result: Result<autonoetic_gateway::SpawnResult, anyhow::Error> = exec
        .spawn_agent_once(
            "leaf",
            "hello",
            "root/child-abc/grandchild-def",
            Some("spawner"),
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await;
    assert!(result.is_err(), "spawn at depth 2 should be rejected");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("max_spawn_depth exceeded"),
        "expected depth error, got: {}",
        msg
    );
    assert!(
        msg.contains("depth 2"),
        "error should mention the child depth, got: {}",
        msg
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn spawn_refused_at_agent_ceiling_when_tighter() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = make_config(&workspace.agents_dir, 8);
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    // Agent declares max_spawn_depth: 1 (tighter than system ceiling of 8)
    install_spawn_agent(&workspace.agents_dir, "shallow-spawner", 10, 1)?;
    install_target_agent(&workspace.agents_dir, "leaf")?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    seed_revision(&store, &config, "shallow-spawner", &workspace.agents_dir)?;
    seed_revision(&store, &config, "leaf", &workspace.agents_dir)?;

    let exec = Arc::new(GatewayExecutionService::new(config.clone(), Some(store)));

    // Session "root" → depth 0: should be allowed (depth 0 < effective ceiling 1)
    let result: Result<autonoetic_gateway::SpawnResult, anyhow::Error> = exec
        .spawn_agent_once(
            "leaf",
            "hello",
            "root",
            Some("shallow-spawner"),
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await;
    match &result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("spawn-chain depth"),
                "depth cap should not trigger at depth 0, but got: {}",
                msg
            );
        }
        Ok(_) => {}
    }

    // Session "root/child-abc" → depth 1: should be REFUSED (depth 1 >= effective ceiling 1)
    let result: Result<autonoetic_gateway::SpawnResult, anyhow::Error> = exec
        .spawn_agent_once(
            "leaf",
            "hello",
            "root/child-abc",
            Some("shallow-spawner"),
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await;
    assert!(
        result.is_err(),
        "spawn at depth 1 should be rejected with agent ceiling 1"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("max_spawn_depth exceeded"),
        "expected depth error, got: {}",
        msg
    );

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn spawn_refused_at_depth_zero_no_capability() -> anyhow::Result<()> {
    let workspace = support::TestWorkspace::new()?;
    let config = make_config(&workspace.agents_dir, 8);
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    // leaf agent has NO AgentSpawn capability
    install_target_agent(&workspace.agents_dir, "no-spawn")?;
    install_target_agent(&workspace.agents_dir, "leaf")?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    seed_revision(&store, &config, "no-spawn", &workspace.agents_dir)?;
    seed_revision(&store, &config, "leaf", &workspace.agents_dir)?;

    let exec = Arc::new(GatewayExecutionService::new(config.clone(), Some(store)));

    let result: Result<autonoetic_gateway::SpawnResult, anyhow::Error> = exec
        .spawn_agent_once(
            "leaf",
            "hello",
            "root",
            Some("no-spawn"),
            false,
            None,
            None,
            None,
            None,
            None,
        &[],
        )
        .await;
    assert!(
        result.is_err(),
        "agent without AgentSpawn should be rejected"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("lacks 'AgentSpawn' capability"),
        "expected capability error, got: {}",
        msg
    );

    Ok(())
}

#[test]
fn policy_spawn_depth_limit_returns_declared_value() {
    let manifest_content = r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "test-agent"
  name: "test"
  description: "test"
capabilities:
  - type: "AgentSpawn"
    max_children: 5
    max_spawn_depth: 3
"#;
    let manifest: AgentManifest = serde_yaml::from_str(manifest_content).unwrap();
    let policy = PolicyEngine::new(manifest);

    assert_eq!(policy.spawn_agent_limit(), Some(5));
    assert_eq!(policy.spawn_depth_limit(), Some(3));
}

#[test]
fn policy_spawn_depth_limit_defaults_to_zero_when_omitted() {
    let manifest_content = r#"
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "test-agent"
  name: "test"
  description: "test"
capabilities:
  - type: "AgentSpawn"
    max_children: 5
"#;
    let manifest: AgentManifest = serde_yaml::from_str(manifest_content).unwrap();
    let policy = PolicyEngine::new(manifest);

    assert_eq!(policy.spawn_depth_limit(), Some(0));
}

#[test]
fn session_depth_computation() {
    use autonoetic_gateway::runtime::live_digest::session_depth;

    assert_eq!(session_depth("root"), 0);
    assert_eq!(session_depth("root/child-abc"), 1);
    assert_eq!(session_depth("root/child-abc/grandchild-def"), 2);
    assert_eq!(session_depth("a/b/c/d/e"), 4);
}
