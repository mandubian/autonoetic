//! Singleton agent dedup integration tests (RFC phase 1).

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::update_task_run_status;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::TaskRunStatus;
use std::sync::Arc;
use tempfile::tempdir;

fn planner_manifest() -> AgentManifest {
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
            id: "planner.default".to_string(),
            name: "planner.default".to_string(),
            description: "test".to_string(),
            singleton: false,
        },
        capabilities: vec![Capability::AgentSpawn {
            max_children: 4,
            max_spawn_depth: 0,
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
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn write_singleton_skill(agent_dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        r#"---
name: "singleton.test"
description: "singleton test agent"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "singleton.test"
      name: "singleton.test"
      description: "singleton test agent"
      singleton: true
---
# Singleton test agent
"#,
    )?;
    Ok(())
}

fn setup() -> anyhow::Result<(tempfile::TempDir, GatewayConfig, Arc<GatewayStore>)> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let planner_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&planner_dir)?;
    write_singleton_skill(&agents_dir.join("singleton.test"))?;

    let config = GatewayConfig {
        agents_dir,
        default_workflow_wait_secs: 10,
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

#[test]
fn singleton_spawn_creates_first_task() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let args = serde_json::json!({
        "agent_id": "singleton.test",
        "message": "do the thing",
        "async": true
    });

    let result = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some("root-singleton-first"),
        Some("turn-singleton-first"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["status"].as_str(), Some("queued"));
    assert!(parsed["task_id"].as_str().is_some());
    Ok(())
}

#[test]
fn singleton_spawn_deduplicates_active_task() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let args = serde_json::json!({
        "agent_id": "singleton.test",
        "message": "do the thing",
        "async": true
    });

    let first = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some("root-singleton-dedup"),
        Some("turn-singleton-dedup-1"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let first_json: serde_json::Value = serde_json::from_str(&first)?;
    let first_task_id = first_json["task_id"].as_str().unwrap().to_string();

    let second = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some("root-singleton-dedup"),
        Some("turn-singleton-dedup-2"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let second_json: serde_json::Value = serde_json::from_str(&second)?;

    assert_eq!(second_json["ok"].as_bool(), Some(true));
    assert_eq!(second_json["status"].as_str(), Some("deduplicated"));
    assert_eq!(second_json["singleton"].as_bool(), Some(true));
    assert_eq!(second_json["deduplicated"].as_bool(), Some(true));
    assert_eq!(
        second_json["task_id"].as_str(),
        Some(first_task_id.as_str())
    );
    Ok(())
}

#[test]
fn singleton_spawn_different_revision_is_separate_task() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let args_a = serde_json::json!({
        "agent_id": "singleton.test",
        "message": "do the thing",
        "async": true,
        "revision_id": "rev-a"
    });
    let first = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args_a)?,
        Some("root-singleton-rev"),
        Some("turn-singleton-rev-1"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let first_json: serde_json::Value = serde_json::from_str(&first)?;
    assert_eq!(first_json["status"].as_str(), Some("queued"));

    let args_b = serde_json::json!({
        "agent_id": "singleton.test",
        "message": "do the thing",
        "async": true,
        "revision_id": "rev-b"
    });
    let second = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args_b)?,
        Some("root-singleton-rev"),
        Some("turn-singleton-rev-2"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let second_json: serde_json::Value = serde_json::from_str(&second)?;
    assert_eq!(second_json["status"].as_str(), Some("queued"));
    assert_ne!(
        second_json["task_id"].as_str(),
        first_json["task_id"].as_str()
    );
    Ok(())
}

#[test]
fn singleton_spawn_after_terminal_creates_new_task() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let _guard = runtime.enter();

    let root_session_id = "root-singleton-terminal";
    let args = serde_json::json!({
        "agent_id": "singleton.test",
        "message": "do the thing",
        "async": true
    });

    let first = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-singleton-terminal-1"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let first_json: serde_json::Value = serde_json::from_str(&first)?;
    let first_task_id = first_json["task_id"].as_str().unwrap().to_string();
    let workflow_id = first_json["workflow_id"].as_str().unwrap().to_string();

    // Mark the first task as succeeded to release the singleton slot.
    update_task_run_status(
        &config,
        Some(store.as_ref()),
        &workflow_id,
        &first_task_id,
        TaskRunStatus::Succeeded,
        Some("done".to_string()),
        None,
        None,
    )?;

    let second = registry.execute(
        "agent_spawn",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-singleton-terminal-2"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let second_json: serde_json::Value = serde_json::from_str(&second)?;
    assert_eq!(second_json["status"].as_str(), Some("queued"));
    assert_ne!(
        second_json["task_id"].as_str(),
        Some(first_task_id.as_str())
    );
    Ok(())
}
