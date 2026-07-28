//! Hibernate-based workflow.wait integration tests (issue #743).
//!
//! Verifies that `workflow.wait` returns immediately with a `waiting_for_child`
//! suspension marker when the join is not yet satisfied, rather than blocking
//! in-tool. The lifecycle layer is responsible for suspending the session and
//! mechanically re-issuing the same `workflow.wait` call on resume.

use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{self, save_task_run, save_workflow_run};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRun, WorkflowRunStatus};
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
            resident_idle_ttl_secs: None,
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        egress: None,
        }
}

fn setup() -> anyhow::Result<(tempfile::TempDir, GatewayConfig, Arc<GatewayStore>)> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let parent_dir = agents_dir.join("planner.default");
    std::fs::create_dir_all(&parent_dir)?;
    let config = GatewayConfig {
        agents_dir,
        default_workflow_wait_secs: 10,
        ..GatewayConfig::default()
    };
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    Ok((temp, config, store))
}

fn seed_two_tasks(
    config: &GatewayConfig,
    store: &GatewayStore,
    workflow_id: &str,
    root_session_id: &str,
    task_a: &str,
    task_b: &str,
) -> anyhow::Result<()> {
    let workflow = WorkflowRun {
        workflow_id: workflow_id.to_string(),
        root_session_id: root_session_id.to_string(),
        lead_agent_id: "planner.default".to_string(),
        status: WorkflowRunStatus::WaitingChildren,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        active_task_ids: vec![],
        queued_task_ids: vec![],
        join_policy: Default::default(),
        join_task_ids: vec![task_a.to_string(), task_b.to_string()],
        active_plan_ref: None,
        reactivated_for_root_spawn: false,
    };
    save_workflow_run(config, Some(store), &workflow)?;

    for tid in [task_a, task_b] {
        let task = TaskRun {
            task_id: tid.to_string(),
            workflow_id: workflow_id.to_string(),
            agent_id: "coder.default".to_string(),
            session_id: format!("{}/{}", root_session_id, tid),
            parent_session_id: root_session_id.to_string(),
            status: TaskRunStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: None,
            result_summary: None,
            join_group: None,
            message: None,
            metadata: None,
            retry_count: 0,
            last_failure_class: None,
            retry_policy: None,
            side_effect_state: None,
            dedupe_key: None,
        };
        save_task_run(config, Some(store), &task)?;
    }
    Ok(())
}

fn execute_workflow_wait(
    config: &GatewayConfig,
    store: Arc<GatewayStore>,
    args: &serde_json::Value,
    session_id: &str,
) -> anyhow::Result<String> {
    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = Arc::new(default_registry());
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(config);
    let args_str = serde_json::to_string(args)?;

    registry.execute(
        "workflow_wait",
        &manifest,
        &policy,
        &parent_dir,
        Some(&gateway_dir),
        &args_str,
        Some(session_id),
        Some("turn-1"),
        Some(config),
        Some(store),
        None,
    )
}

/// Probe mode (timeout_secs=0) returns immediately without suspending.
#[test]
fn timeout_zero_returns_immediately() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-probe";
    let root_session_id = "root-probe";
    let task_a = "task-probe-a";
    let task_b = "task-probe-b";
    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_a, task_b)?;

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_a, task_b],
        "timeout_secs": 0
    });

    let result = execute_workflow_wait(&config, store, &args, root_session_id)?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(false));
    assert!(
        parsed.get("waiting_for_child").is_none(),
        "probe mode should not set waiting_for_child"
    );
    Ok(())
}

/// Blocking mode returns a suspension marker when the join is not satisfied.
#[test]
fn blocking_mode_suspends_when_tasks_running() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-suspend";
    let root_session_id = "root-suspend";
    let task_a = "task-suspend-a";
    let task_b = "task-suspend-b";
    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_a, task_b)?;

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_a, task_b],
        "timeout_secs": 30
    });

    let start = std::time::Instant::now();
    let result = execute_workflow_wait(&config, store, &args, root_session_id)?;
    let elapsed = start.elapsed();

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(false));
    assert_eq!(
        parsed["waiting_for_child"].as_bool(),
        Some(true),
        "workflow_wait must return waiting_for_child when join is not satisfied"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "workflow_wait must return immediately, not block; got {:?}",
        elapsed
    );
    Ok(())
}

/// Blocking mode returns join_satisfied when all watched tasks are already terminal.
#[test]
fn blocking_mode_returns_terminal_immediately() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-terminal";
    let root_session_id = "root-terminal";
    let task_a = "task-terminal-a";
    let task_b = "task-terminal-b";
    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_a, task_b)?;

    for tid in [task_a, task_b] {
        workflow_store::update_task_run_status(
            &config,
            Some(store.as_ref()),
            workflow_id,
            tid,
            TaskRunStatus::Succeeded,
            Some("done".to_string()),
            None,
            None,
        )?;
    }

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_a, task_b],
        "timeout_secs": 30
    });

    let result = execute_workflow_wait(&config, store, &args, root_session_id)?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(true));
    assert_ne!(
        parsed["waiting_for_child"].as_bool(),
        Some(true),
        "terminal join should not set waiting_for_child"
    );
    Ok(())
}

/// Workflow ID resolution from the current root session works in both modes.
#[test]
fn resolves_workflow_id_from_session() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-resolve";
    let root_session_id = "root-resolve";
    let task_a = "task-resolve-a";
    let task_b = "task-resolve-b";
    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_a, task_b)?;
    store.set_workflow_index(root_session_id, workflow_id)?;

    let args = serde_json::json!({
        "task_ids": [task_a, task_b],
        "timeout_secs": 0
    });

    let result = execute_workflow_wait(&config, store, &args, root_session_id)?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["workflow_id"].as_str(), Some(workflow_id));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(false));
    Ok(())
}
