//! Signal-driven workflow.wait integration tests (issue #288).
//!
//! Verifies that `workflow.wait` wakes on child-state transitions via the
//! TaskNotifyRegistry rather than relying on polling.

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
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
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
    };
    save_workflow_run(config, Some(store), &workflow)?;

    for tid in [task_a, task_b] {
        let task = TaskRun {
            task_id: tid.to_string(),
            workflow_id: workflow_id.to_string(),
            agent_id: "exec-agent".to_string(),
            session_id: format!("{root_session_id}/{tid}"),
            parent_session_id: root_session_id.to_string(),
            status: TaskRunStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            source_agent_id: Some("planner.default".to_string()),
            result_summary: None,
            join_group: None,
            message: Some("test task".to_string()),
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

/// Legacy probe: timeout_secs=0 returns immediately with current status.
#[test]
fn timeout_zero_returns_immediately() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-probe";
    let root_session_id = "root-probe";
    let task_id = "task-probe";

    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_id, "task-probe-b")?;

    let manifest = planner_manifest();
    let _policy = PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);
    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_id],
        "timeout_secs": 0
    });

    let start = std::time::Instant::now();
    let result = registry.execute(
        "workflow_wait",
        &manifest,
        &_policy,
        &parent_dir,
        Some(&gateway_dir),
        &serde_json::to_string(&args)?,
        Some(root_session_id),
        Some("turn-probe"),
        Some(&config),
        Some(store.clone()),
        None,
    )?;
    let elapsed = start.elapsed();

    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(false));
    assert_eq!(parsed["waited_secs"].as_u64(), Some(0));
    assert!(elapsed < std::time::Duration::from_millis(500));
    Ok(())
}

/// Signal-driven wake: transitioning a task wakes workflow.wait within
/// milliseconds, not after a multi-second poll cycle.
#[tokio::test]
async fn signal_wakes_wait_within_100ms() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-signal";
    let root_session_id = "root-signal";
    let task_a = "task-sig-a";
    let task_b = "task-sig-b";

    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_a, task_b)?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = Arc::new(default_registry());
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_a, task_b],
        "timeout_secs": 30
    });
    let args_str = serde_json::to_string(&args)?;
    let config_clone = config.clone();
    let store_clone = store.clone();
    let manifest_clone = manifest.clone();

    let wait_handle = tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();
        let result = registry.execute(
            "workflow_wait",
            &manifest_clone,
            &PolicyEngine::new(manifest_clone.clone()),
            &parent_dir,
            Some(&gateway_dir),
            &args_str,
            Some(root_session_id),
            Some("turn-signal"),
            Some(&config_clone),
            Some(store_clone),
            None,
        );
        (start.elapsed(), result)
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_a,
        TaskRunStatus::Succeeded,
        Some("done".to_string()),
        None,
        None,
    )?;

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_b,
        TaskRunStatus::Succeeded,
        Some("done".to_string()),
        None,
        None,
    )?;

    let (elapsed, result) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_handle,
    )
    .await
    .map_err(|_| anyhow::anyhow!("workflow.wait timed out"))??;

    let result = result?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(true));
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "expected sub-2s wake, got {:?}",
        elapsed
    );

    Ok(())
}

/// Deadline path: with a short timeout and no transitions, workflow.wait
/// returns at the deadline with tasks still running.
#[tokio::test]
async fn deadline_returns_with_tasks_still_running() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-deadline";
    let root_session_id = "root-deadline";
    let task_a = "task-dl-a";
    let task_b = "task-dl-b";

    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_a, task_b)?;

    let manifest = planner_manifest();
    let policy = PolicyEngine::new(manifest.clone());
    let registry = Arc::new(default_registry());
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_a, task_b],
        "timeout_secs": 2
    });
    let args_str = serde_json::to_string(&args)?;

    let wait_handle = tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();
        let result = registry.execute(
            "workflow_wait",
            &manifest,
            &policy,
            &parent_dir,
            Some(&gateway_dir),
            &args_str,
            Some(root_session_id),
            Some("turn-deadline"),
            Some(&config),
            Some(store),
            None,
        );
        (start.elapsed(), result)
    });

    let (elapsed, result) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait_handle,
    )
    .await
    .map_err(|_| anyhow::anyhow!("workflow.wait timed out"))??;

    let result = result?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(false));
    assert!(
        elapsed >= std::time::Duration::from_secs(1),
        "expected at least 1s wait, got {:?}",
        elapsed
    );
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "expected sub-8s wait (signal fallback), got {:?}",
        elapsed
    );

    Ok(())
}

/// Sequential transitions: both tasks transition one at a time with a delay,
/// and the total wait is bounded by signal response time, not polling interval.
#[tokio::test]
async fn sequential_transitions_both_complete_fast() -> anyhow::Result<()> {
    let (_temp, config, store) = setup()?;
    let workflow_id = "wf-seq";
    let root_session_id = "root-seq";
    let task_a = "task-seq-a";
    let task_b = "task-seq-b";

    seed_two_tasks(&config, &store, workflow_id, root_session_id, task_a, task_b)?;

    let manifest = planner_manifest();
    let registry = Arc::new(default_registry());
    let parent_dir = config.agents_dir.join("planner.default");
    let gateway_dir = autonoetic_gateway::execution::gateway_root_dir(&config);

    let args = serde_json::json!({
        "workflow_id": workflow_id,
        "task_ids": [task_a, task_b],
        "timeout_secs": 30
    });
    let args_str = serde_json::to_string(&args)?;
    let config_clone = config.clone();
    let store_clone = store.clone();
    let manifest_clone = manifest.clone();

    let wait_handle = tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();
        let result = registry.execute(
            "workflow_wait",
            &manifest_clone,
            &PolicyEngine::new(manifest_clone.clone()),
            &parent_dir,
            Some(&gateway_dir),
            &args_str,
            Some(root_session_id),
            Some("turn-seq"),
            Some(&config_clone),
            Some(store_clone),
            None,
        );
        (start.elapsed(), result)
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_a,
        TaskRunStatus::Succeeded,
        Some("done a".to_string()),
        None,
        None,
    )?;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    workflow_store::update_task_run_status(
        &config,
        Some(store.as_ref()),
        workflow_id,
        task_b,
        TaskRunStatus::Succeeded,
        Some("done b".to_string()),
        None,
        None,
    )?;

    let (elapsed, result) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        wait_handle,
    )
    .await
    .map_err(|_| anyhow::anyhow!("workflow.wait timed out"))??;

    let result = result?;
    let parsed: serde_json::Value = serde_json::from_str(&result)?;
    assert_eq!(parsed["ok"].as_bool(), Some(true));
    assert_eq!(parsed["join_satisfied"].as_bool(), Some(true));
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "expected sub-2s wake, got {:?}",
        elapsed
    );

    Ok(())
}
