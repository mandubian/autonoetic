//! C2 / #616 — Root-session-tree budget exhaustion cascades to descendants.
//!
//! When the root-tree budget (P-6.21) is exhausted, the parent's next LLM
//! call is blocked but in-flight descendant sessions keep running and keep
//! burning the already-spent tree budget. The graceful root budget circuit
//! breaker reuses the emergency-stop cascade to cancel those descendants.
//!
//! These tests cover:
//!   1. Root-budget exhaustion with ≥2 in-flight descendants cancels them all
//!      (descendant workflow tasks aborted) and writes a single
//!      `EmergencyStopRecord` with the budget reason / `root_budget_exhausted`
//!      trigger. Cancelling the descendants is what stops further tree-budget
//!      burn; we assert the cancellation + stop record (the mechanism), not a
//!      spend counter.
//!   2. Idempotency (sequential): a second root-budget exhaustion (e.g. a
//!      sibling) does NOT create a second cascade / second stop record.
//!   3. Idempotency (concurrent): many siblings hitting the exhausted shared
//!      budget at once still produce exactly one stop record — the atomic
//!      in-process claim closes the check-then-act race.
//!   4. Per-session budget exhaustion does NOT arm the breaker (the
//!      `root_budget_exhausted` flag stays false), so no root cascade fires.

mod support;

use std::sync::Arc;

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::root_session_budget::RootSessionBudgetRegistry;
use autonoetic_gateway::runtime::session_budget::SessionBudgetRegistry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, list_task_runs_for_workflow, load_workflow_run,
    save_task_run, save_workflow_run,
};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig, RuntimeDeclaration};
use autonoetic_types::config::{RootSessionBudgetConfig, SessionBudgetConfig};
use autonoetic_types::workflow::{TaskRun, TaskRunStatus, WorkflowRunStatus};
use chrono::Utc;
use support::TestWorkspace;

// --------------------------------------------------------------------------
// shared helpers
// --------------------------------------------------------------------------

struct NoOpDriver;

#[async_trait::async_trait]
impl LlmDriver for NoOpDriver {
    async fn complete(
        &self,
        _request: &CompletionRequest,
    ) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: "ok".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

fn test_manifest() -> AgentManifest {
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
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
            singleton: false,
        },
        capabilities: vec![],
        llm_overrides: None,
        llm_preset: None,
        llm_config: Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.2,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
        }),
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

/// Build a workflow for `root_session` with two in-flight (Running) descendant
/// tasks, mirroring the emergency-stop integration scaffolding.
fn seed_workflow_with_two_running_children(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
    root_session: &str,
) -> anyhow::Result<String> {
    let mut wf = ensure_workflow_for_root_session(
        config,
        Some(store),
        root_session,
        Some("planner.default"),
    )?;
    wf.status = WorkflowRunStatus::WaitingChildren;
    wf.updated_at = Utc::now().to_rfc3339();
    save_workflow_run(config, Some(store), &wf)?;

    let ts = Utc::now().to_rfc3339();
    for tid in ["task-a", "task-b"] {
        let task = TaskRun {
            task_id: tid.to_string(),
            workflow_id: wf.workflow_id.clone(),
            agent_id: "coder.default".to_string(),
            session_id: format!("{root_session}/child-{tid}"),
            parent_session_id: root_session.to_string(),
            status: TaskRunStatus::Running,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            source_agent_id: Some("planner.default".to_string()),
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
    Ok(wf.workflow_id)
}

// --------------------------------------------------------------------------
// 1. Cascade: descendants cancelled + stop record + spend does not climb
// --------------------------------------------------------------------------

#[serial_test::serial]
#[tokio::test]
async fn root_budget_breaker_cancels_descendants_and_records_stop() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let root_session = "root-c2-cascade";
    let workflow_id = seed_workflow_with_two_running_children(&config, &store, root_session)?;

    // Pre-condition: both descendants are in-flight (Running).
    let before = list_task_runs_for_workflow(&config, Some(store.as_ref()), &workflow_id)?;
    assert_eq!(before.len(), 2);
    assert!(before.iter().all(|t| t.status == TaskRunStatus::Running));

    // Fire the breaker as the production seam would.
    let underlying = anyhow::anyhow!(
        "Root session budget exceeded: max_llm_rounds (4, would be 5) (root: {root_session})"
    );
    execution
        .trigger_root_budget_circuit_breaker(root_session, &underlying)
        .await;

    // All descendants cancelled (aborted).
    let after = list_task_runs_for_workflow(&config, Some(store.as_ref()), &workflow_id)?;
    assert_eq!(after.len(), 2);
    for t in &after {
        assert_eq!(
            t.status,
            TaskRunStatus::Aborted,
            "descendant task {} should be aborted",
            t.task_id
        );
    }

    // Workflow itself moves to EmergencyStopped.
    let run = load_workflow_run(&config, Some(store.as_ref()), &workflow_id)?.expect("workflow");
    assert_eq!(run.status, WorkflowRunStatus::EmergencyStopped);

    // Exactly one emergency-stop record exists for the root, attributed to the
    // budget circuit breaker with the budget reason.
    let stops = store.list_emergency_stops_for_root_session(root_session)?;
    assert_eq!(stops.len(), 1, "exactly one stop record for the root");
    let stop = &stops[0];
    assert_eq!(stop.trigger_kind, "root_budget_exhausted");
    assert_eq!(stop.requested_by_type, "gateway");
    assert_eq!(stop.requested_by_id, "root_budget_circuit_breaker");
    let reason = stop.reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("Root session budget exhausted"),
        "stop reason should cite budget exhaustion, got: {reason}"
    );
    assert!(
        reason.contains("P-6.21"),
        "stop reason should cite P-6.21, got: {reason}"
    );

    // Checkpoint reflects the emergency stop (do-not-resume).
    let cp = load_latest_checkpoint(&config, root_session)?.expect("checkpoint");
    match &cp.yield_reason {
        YieldReason::EmergencyStop { stop_id } => assert_eq!(stop_id, &stop.stop_id),
        other => panic!("expected EmergencyStop checkpoint, got {other:?}"),
    }

    // Tree spend does not climb after the event: once stopped, descendants are
    // cancelled and the pre-flight guard makes siblings yield EmergencyStop
    // rather than reserving more budget. Re-firing is a no-op (see idempotency
    // test); here we assert the cascade left exactly the one stop record so no
    // further budget-burning turns can start under this root.
    let stops_again = store.list_emergency_stops_for_root_session(root_session)?;
    assert_eq!(stops_again.len(), 1);

    Ok(())
}

// --------------------------------------------------------------------------
// 2. Idempotent: a second exhaustion (sibling) does not double-cascade
// --------------------------------------------------------------------------

#[serial_test::serial]
#[tokio::test]
async fn root_budget_breaker_is_idempotent_per_root() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let root_session = "root-c2-idempotent";
    seed_workflow_with_two_running_children(&config, &store, root_session)?;

    let err1 = anyhow::anyhow!("Root session budget exceeded: max_llm_tokens (root: parent)");
    let err2 = anyhow::anyhow!("Root session budget exceeded: max_llm_tokens (root: sibling)");

    execution
        .trigger_root_budget_circuit_breaker(root_session, &err1)
        .await;
    let after_first = store.list_emergency_stops_for_root_session(root_session)?;
    assert_eq!(after_first.len(), 1, "first call creates one stop record");
    let first_stop_id = after_first[0].stop_id.clone();

    // Second exhaustion (e.g. a sibling session that also hit the exhausted
    // tree budget) must not create a second cascade / second stop record.
    execution
        .trigger_root_budget_circuit_breaker(root_session, &err2)
        .await;
    let after_second = store.list_emergency_stops_for_root_session(root_session)?;
    assert_eq!(
        after_second.len(),
        1,
        "second call must not create a second stop record"
    );
    assert_eq!(
        after_second[0].stop_id, first_stop_id,
        "the single stop record is unchanged"
    );

    Ok(())
}

/// Idempotency under concurrency — the realistic case: a shared tree budget is
/// exhausted, so many sibling sessions hit the limit at once and each fires the
/// breaker concurrently. The atomic in-process claim must ensure exactly one
/// cascade / one stop record (without it, the check-then-act on the DB lookup
/// races and produces duplicates).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_budget_breaker_is_idempotent_under_concurrency() -> anyhow::Result<()> {
    let workspace = TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let execution = Arc::new(GatewayExecutionService::new(
        config.clone(),
        Some(store.clone()),
    ));

    let root_session = "root-c2-concurrent";
    seed_workflow_with_two_running_children(&config, &store, root_session)?;

    // Fan out N concurrent siblings that all hit the exhausted tree budget.
    let mut handles = Vec::new();
    for i in 0..8 {
        let exec = execution.clone();
        let root = root_session.to_string();
        handles.push(tokio::spawn(async move {
            let err = anyhow::anyhow!(
                "Root session budget exceeded: max_llm_tokens (sibling {i})"
            );
            exec.trigger_root_budget_circuit_breaker(&root, &err).await;
        }));
    }
    for h in handles {
        h.await?;
    }

    let stops = store.list_emergency_stops_for_root_session(root_session)?;
    assert_eq!(
        stops.len(),
        1,
        "concurrent siblings must create exactly one stop record (atomic claim closes the race)"
    );

    Ok(())
}

// --------------------------------------------------------------------------
// 3. Per-session budget exhaustion does NOT arm the breaker
// --------------------------------------------------------------------------

/// The breaker is keyed off `AgentExecutor::root_budget_exhausted`, which is
/// set ONLY by the root-tree budget paths. Per-session budget exhaustion must
/// leave it false so no root cascade ever fires.
#[tokio::test]
async fn per_session_budget_exhaustion_does_not_arm_breaker() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut executor = AgentExecutor::new(
        test_manifest(),
        "System prompt".to_string(),
        Arc::new(NoOpDriver),
        temp.path().to_path_buf(),
        autonoetic_gateway::runtime::tools::default_registry(),
        None,
    );
    executor.session_id = Some("root-per-session/child-1".to_string());

    // Per-session budget exhausted (0 llm rounds), root-tree budget generous.
    executor.session_budget = Some(Arc::new(SessionBudgetRegistry::new(SessionBudgetConfig {
        max_llm_rounds: Some(0),
        ..Default::default()
    })));
    executor.root_session_budget = Some(Arc::new(RootSessionBudgetRegistry::new(
        RootSessionBudgetConfig {
            max_llm_rounds: Some(1000),
            ..Default::default()
        },
    )));

    let mut history = vec![];
    let result = executor.pre_turn_checks(&mut history, "turn-000001").await;

    assert!(
        result.is_err(),
        "pre_turn_checks must error when the per-session budget is exhausted"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Session budget exceeded"),
        "expected per-session budget error, got: {err}"
    );
    assert!(
        !executor.root_budget_exhausted,
        "per-session budget exhaustion must NOT arm the root budget breaker"
    );

    Ok(())
}

/// Conversely: root-tree budget exhaustion DOES arm the breaker flag.
#[tokio::test]
async fn root_budget_exhaustion_arms_breaker_flag() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let mut executor = AgentExecutor::new(
        test_manifest(),
        "System prompt".to_string(),
        Arc::new(NoOpDriver),
        temp.path().to_path_buf(),
        autonoetic_gateway::runtime::tools::default_registry(),
        None,
    );
    executor.session_id = Some("root-tree-exhausted/child-1".to_string());

    // Per-session budget generous; root-tree wall-clock budget already blown
    // (max_wall_clock_secs = 0 trips on the very first check_pre_llm).
    executor.session_budget = Some(Arc::new(SessionBudgetRegistry::new(SessionBudgetConfig {
        max_llm_rounds: Some(1000),
        ..Default::default()
    })));
    executor.root_session_budget = Some(Arc::new(RootSessionBudgetRegistry::new(
        RootSessionBudgetConfig {
            max_wall_clock_secs: Some(0),
            ..Default::default()
        },
    )));

    let mut history = vec![];
    let result = executor.pre_turn_checks(&mut history, "turn-000001").await;

    assert!(
        result.is_err(),
        "pre_turn_checks must error when the root-tree budget is exhausted"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Root session budget exceeded"),
        "expected root-tree budget error, got: {err}"
    );
    assert!(
        executor.root_budget_exhausted,
        "root-tree budget exhaustion MUST arm the breaker flag"
    );

    Ok(())
}
