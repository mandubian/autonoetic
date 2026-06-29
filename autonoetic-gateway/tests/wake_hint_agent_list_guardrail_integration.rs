//! Integration tests for the post-approval wake-hint guardrail on `agent_list`.
//!
//! Background: when a plan is approved, the operator's TUI sends a wake
//! message to the planner via `ChatOutbound::PlanApprovalWake`. The gateway
//! registers a per-root-session `WakeHintState` and threads it into the
//! `NativeToolRunContext`. While the hint is active, `agent_list` must
//! mechanically return an error directing the planner to the explicit
//! `agent_id` in the wake message — this prevents the post-approval
//! roster loop that triggered LoopGuard degradation in
//! `~/.autonoetic/agents/.gateway/sessions/latest/digest.md`.

use autonoetic_gateway::execution::WakeHintState;
use autonoetic_gateway::runtime::active_execution_registry::ActiveExecutionRegistry;
use autonoetic_gateway::runtime::active_execution_registry::NativeToolRunContext;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration, ScriptInputMode};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::tempdir;

fn make_manifest() -> AgentManifest {
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
            id: "planner.collaborative".to_string(),
            name: "Collaborative Planner".to_string(),
            description: "Test collaborative planner".to_string(),
            singleton: false,
        },
        capabilities: vec![
            Capability::AgentSpawn {
                max_children: 10,
                max_spawn_depth: 0,
            },
            Capability::SandboxFunctions {
                allowed: vec!["agent.".to_string()],
            },
        ],
        llm_overrides: None,
        llm_preset: None,
        llm_config: None,
        limits: None,
        background: None,
        disclosure: None,
        io: None,
        execution_mode: ExecutionMode::default(),
        script_entry: None,
        script_input_mode: ScriptInputMode::default(),
        gateway_url: None,
        gateway_token: None,
        middleware: None,
        agentskills_import: None,
        allowed_tool_tiers: vec![],
        compression: None,
            open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn make_run_context(root_session_id: &str, wake_hint: Option<WakeHintState>) -> NativeToolRunContext {
    let wake_hints_map: Arc<Mutex<HashMap<String, WakeHintState>>> =
        Arc::new(Mutex::new(HashMap::new()));
    if let Some(ref hint) = wake_hint {
        // Synchronous test setup — blocking_lock would deadlock, but
        // try_lock is fine since the map is uncontended.
        if let Ok(mut guard) = wake_hints_map.try_lock() {
            guard.insert(root_session_id.to_string(), hint.clone());
        }
    }
    NativeToolRunContext {
        registry: ActiveExecutionRegistry::new(),
        root_session_id: root_session_id.to_string(),
        workflow_id: Some("wf-1".to_string()),
        task_id: Some("task-1".to_string()),
        session_id: format!("{}/planner-001", root_session_id),
        agent_id: "planner.collaborative".to_string(),
        live_digest: None,
        live_report: None,
        user_id: None,
        artifact_id: None,
        sentinel_suppress_target: None,
        discovered_tools: None,
        wake_hint,
        wake_hints_map: Some(wake_hints_map),
    }
}

#[test]
fn agent_list_with_active_wake_hint_returns_blocking_error() {
    let dir = tempdir().unwrap();
    let manifest = make_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let root = "root-wake-001";

    let wake_hint = WakeHintState {
        plan_id: "plan-abc123".to_string(),
        plan_version: 5,
        agent_id: "researcher.default".to_string(),
        step_id: "s1".to_string(),
        delivered_at_turn: 0,
        expires_at_turn: 1,
    };
    let run_context = make_run_context(root, Some(wake_hint.clone()));
    let args = json!({});

    let mut config = GatewayConfig::default();
    config.agents_dir = dir.path().to_path_buf();
    let config_arc = Arc::new(config);

    let result = registry
        .execute(
            "agent_list",
            &manifest,
            &policy,
            dir.path(),
            None,
            &serde_json::to_string(&args).unwrap(),
            Some(&run_context.session_id),
            Some("turn-001"),
            Some(&config_arc),
            None,
            Some(&run_context),
        )
        .unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], false, "agent_list should be blocked by wake hint");
    assert_eq!(parsed["error"], "post_approval_wake_active");
    assert!(parsed["message"]
        .as_str()
        .unwrap()
        .contains("researcher.default"));
    assert_eq!(parsed["hint"]["plan_id"], "plan-abc123");
    assert_eq!(parsed["hint"]["plan_version"], 5);
    assert_eq!(parsed["hint"]["step_id"], "s1");
    assert_eq!(parsed["hint"]["agent_id"], "researcher.default");
}

#[tokio::test]
async fn wake_hint_registration_and_active_lookup() {
    // Smoke test for the wake-hint lifecycle on the
    // GatewayExecutionService: register a hint, look it up while
    // active, and verify it expires after the turn window.
    use autonoetic_gateway::execution::GatewayExecutionService;
    use autonoetic_gateway::scheduler::gateway_store::GatewayStore;

    let dir = tempdir().unwrap();
    let mut config = autonoetic_types::config::GatewayConfig::default();
    config.agents_dir = dir.path().to_path_buf();
    let store = std::sync::Arc::new(
        GatewayStore::open(&dir.path().join(".gateway")).unwrap(),
    );
    let exec = GatewayExecutionService::new(config, Some(store));

    let root = "root-wake-lifecycle-001";
    exec.register_wake_hint(
        root,
        WakeHintState {
            plan_id: "plan-xyz".to_string(),
            plan_version: 3,
            agent_id: "coder.default".to_string(),
            step_id: "s1".to_string(),
            delivered_at_turn: 0,
            expires_at_turn: 1,
        },
    )
    .await;

    // Active at turn 0 (delivered_at_turn=0, expires_at_turn=1).
    let active = exec.active_wake_hint(root, 0).await;
    assert!(active.is_some(), "hint should be active at turn 0");
    let hint = active.unwrap();
    assert_eq!(hint.plan_id, "plan-xyz");
    assert_eq!(hint.plan_version, 3);
    assert_eq!(hint.agent_id, "coder.default");
    assert_eq!(hint.step_id, "s1");

    // Active at turn 1 (expires_at_turn=1, check is `current_turn <= expires_at_turn`).
    let active = exec.active_wake_hint(root, 1).await;
    assert!(active.is_some(), "hint should be active at turn 1");

    // Expired at turn 2.
    let active = exec.active_wake_hint(root, 2).await;
    assert!(active.is_none(), "hint should expire at turn 2");

    // Clear and verify removal.
    exec.clear_wake_hint(root).await;
    let active = exec.active_wake_hint(root, 0).await;
    assert!(active.is_none(), "hint should be cleared");
}

#[test]
fn agent_list_without_wake_hint_falls_through() {
    // When no wake hint is active, agent_list must NOT return the
    // wake-block error. It may return a directory (if agents are
    // configured) or a config error (if not). The key assertion is
    // that the post-approval guardrail does NOT fire.
    let dir = tempdir().unwrap();
    let manifest = make_manifest();
    let policy = autonoetic_gateway::policy::PolicyEngine::new(manifest.clone());
    let registry = default_registry();
    let root = "root-no-wake-001";

    let run_context = make_run_context(root, None);
    let args = json!({});

    // The tool may return an Err if config is missing — that's fine.
    // The key check is that it does NOT return the wake-block error.
    let result = registry.execute(
        "agent_list",
        &manifest,
        &policy,
        dir.path(),
        None,
        &serde_json::to_string(&args).unwrap(),
        Some(&run_context.session_id),
        Some("turn-001"),
        None,
        None,
        Some(&run_context),
    );

    match result {
        Ok(json_str) => {
            let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
            if let Some(err) = parsed.get("error") {
                assert_ne!(
                    err.as_str().unwrap(),
                    "post_approval_wake_active",
                    "agent_list must not be blocked when no wake hint is active"
                );
            }
        }
        Err(_) => {
            // Err is acceptable here (e.g. missing config). The point is
            // the wake-block error must not be returned.
        }
    }
}
