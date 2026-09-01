//! Isolated tests for the `AgentExecutor` helper methods extracted in #566.

use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage, ToolCall,
    ToolDefinition,
};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::disclosure::DisclosureState;
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::mcp::McpToolRuntime;
use autonoetic_gateway::runtime::session_tracer::SessionTracer;
use autonoetic_gateway::runtime::tools::{NativeTool, NativeToolRegistry};
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::SessionBudgetConfig;
use autonoetic_types::disclosure::DisclosurePolicy;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

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

fn manifest_with_capabilities(capabilities: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "test-agent".to_string(),
            name: "test-agent".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities,
        llm_config: Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.2,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            max_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        }),
        ..TestManifest::new().build()
    }
}

fn empty_executor() -> (AgentExecutor, tempfile::TempDir) {
    let temp = tempdir().expect("tempdir should create");
    let manifest = manifest_with_capabilities(vec![]);
    let executor = AgentExecutor::new(
        manifest,
        "System prompt".to_string(),
        Arc::new(NoOpDriver),
        temp.path().to_path_buf(),
        autonoetic_gateway::runtime::tools::default_registry(),
        None,
    );
    (executor, temp)
}

#[tokio::test]
async fn pre_turn_checks_yields_error_when_session_budget_exhausted() {
    let (mut executor, _temp) = empty_executor();
    executor.session_id = Some("sess-budget".to_string());
    executor.session_budget = Some(Arc::new(
        autonoetic_gateway::runtime::session_budget::SessionBudgetRegistry::new(
            SessionBudgetConfig {
                profile: None,
                max_llm_rounds: Some(0),
                max_tool_invocations: None,
                max_llm_tokens: None,
                max_wall_clock_secs: None,
                max_session_price_usd: None,
                extensions: Default::default(),
            },
        ),
    ));

    let mut history = vec![];
    let result = executor
        .pre_turn_checks(&mut history, "turn-000001")
        .await;

    assert!(
        result.is_err(),
        "pre_turn_checks must error when the session budget is exhausted"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Session budget exceeded"),
        "expected budget error, got: {err}"
    );
}

#[tokio::test]
async fn pre_turn_checks_yields_error_on_emergency_stop() {
    let (mut executor, temp) = empty_executor();
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
    let store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)
            .expect("gateway store should open"),
    );

    let session_id = "root-emergency/agent.default-1234";
    executor.session_id = Some(session_id.to_string());
    executor.gateway_store = Some(store.clone());

    let root_session_id = autonoetic_gateway::runtime::content_store::root_session_id(session_id);
    store
        .insert_emergency_stop(&autonoetic_gateway::scheduler::gateway_store::EmergencyStopRecord {
            stop_id: "stop-test".to_string(),
            scope_type: "root_session".to_string(),
            scope_id: root_session_id.to_string(),
            root_session_id: root_session_id.to_string(),
            workflow_id: None,
            requested_by_type: "test".to_string(),
            requested_by_id: "test".to_string(),
            reason: Some("test stop".to_string()),
            trigger_kind: "operator".to_string(),
            mode: "immediate".to_string(),
            status: "active".to_string(),
            requested_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
            details_json: None,
        })
        .expect("insert emergency stop should succeed");

    let mut history = vec![];
    let result = executor
        .pre_turn_checks(&mut history, "turn-000001")
        .await;

    assert!(
        result.is_err(),
        "pre_turn_checks must error when the root session is emergency-stopped"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("emergency_stop"),
        "expected emergency stop error, got: {err}"
    );
}

#[tokio::test]
async fn pre_turn_checks_returns_none_when_no_gate_trips() {
    let (mut executor, _temp) = empty_executor();
    executor.session_id = Some("sess-normal".to_string());

    let mut history = vec![];
    let outcome = executor
        .pre_turn_checks(&mut history, "turn-000001")
        .await
        .expect("pre_turn_checks should succeed");

    assert!(
        outcome.is_none(),
        "pre_turn_checks should return None when no gate trips"
    );
}

struct ApprovalLifecycleTool;

impl NativeTool for ApprovalLifecycleTool {
    fn name(&self) -> &'static str {
        "test.approval"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Lifecycle approval test tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&autonoetic_gateway::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        Ok(serde_json::json!({
            "ok": false,
            "approval_required": true,
            "request_id": "apr-helper1234"
        })
        .to_string())
    }
}

struct UserAskLifecycleTool;

impl NativeTool for UserAskLifecycleTool {
    fn name(&self) -> &'static str {
        "test.user_ask"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Lifecycle user ask test tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&autonoetic_gateway::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        Ok(serde_json::json!({
            "ok": false,
            "interaction_required": true,
            "interaction_id": "ui-helper1234"
        })
        .to_string())
    }
}

struct EscalationLifecycleTool;

impl NativeTool for EscalationLifecycleTool {
    fn name(&self) -> &'static str {
        "test.escalation"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Lifecycle escalation test tool".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&autonoetic_types::config::GatewayConfig>,
        _gateway_store: Option<Arc<autonoetic_gateway::scheduler::gateway_store::GatewayStore>>,
        _run_context: Option<&autonoetic_gateway::runtime::active_execution_registry::NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        Ok(serde_json::json!({
            "ok": false,
            "escalation_required": true,
            "request_id": "esc-helper1234"
        })
        .to_string())
    }
}

async fn run_handle_tool_batch_with_tool(
    tool: Box<dyn NativeTool>,
    tool_name: &str,
) -> TurnOutcome {
    let temp = tempdir().expect("tempdir should create");
    let manifest = manifest_with_capabilities(vec![]);
    let mut registry = NativeToolRegistry::new();
    registry.register(tool);

    let mut executor = AgentExecutor::new(
        manifest,
        "System prompt".to_string(),
        Arc::new(NoOpDriver),
        temp.path().to_path_buf(),
        registry,
        None,
    );
    executor.session_id = Some("sess-tool".to_string());

    let mut tracer = SessionTracer::new(
        temp.path(),
        &executor.manifest.agent.id,
        "sess-tool",
    )
    .expect("tracer should create");
    let mut mcp_runtime = McpToolRuntime::empty();
    let mut disclosure_state = DisclosureState::new(DisclosurePolicy::default());
    let mut digest_turn_active = true;

    let tool_call = ToolCall {
        id: "tc1".to_string(),
        name: tool_name.to_string(),
        arguments: "{}".to_string(),
    };
    let mut assistant_msg = Message::assistant("trying tool");
    assistant_msg.tool_calls = vec![tool_call.clone()];

    let outcome = executor
        .handle_tool_batch(
            vec![tool_call],
            &mut vec![],
            "turn-000001",
            &mut tracer,
            &mut mcp_runtime,
            &mut disclosure_state,
            None,
            temp.path(),
            assistant_msg,
            &mut digest_turn_active,
        )
        .await
        .expect("handle_tool_batch should succeed");

    outcome.expect("handle_tool_batch should return a suspension outcome")
}

#[tokio::test]
async fn handle_tool_batch_suspends_on_approval_required() {
    let outcome = run_handle_tool_batch_with_tool(
        Box::new(ApprovalLifecycleTool),
        "test.approval",
    )
    .await;

    match outcome {
        TurnOutcome::Suspended {
            approval_request_id,
        } => {
            assert_eq!(approval_request_id, "apr-helper1234");
        }
        other => panic!("expected Suspended, got {:?}", other),
    }
}

#[tokio::test]
async fn handle_tool_batch_suspends_on_user_input_required() {
    let outcome = run_handle_tool_batch_with_tool(
        Box::new(UserAskLifecycleTool),
        "test.user_ask",
    )
    .await;

    match outcome {
        TurnOutcome::SuspendedUserInput { interaction_id } => {
            assert_eq!(interaction_id, "ui-helper1234");
        }
        other => panic!("expected SuspendedUserInput, got {:?}", other),
    }
}

#[tokio::test]
async fn handle_tool_batch_suspends_on_human_escalation() {
    let outcome = run_handle_tool_batch_with_tool(
        Box::new(EscalationLifecycleTool),
        "test.escalation",
    )
    .await;

    match outcome {
        TurnOutcome::Escalated {
            escalation_request_id,
        } => {
            assert_eq!(escalation_request_id, "esc-helper1234");
        }
        other => panic!("expected Escalated, got {:?}", other),
    }
}

#[test]
fn critical_sentinel_emits_operator_activity_not_user_interaction() {
    use autonoetic_gateway::runtime::trajectory_health::{
        DivergenceSignal, DivergenceSignalKind, SignalSeverity, TrajectoryHealth,
    };
    use autonoetic_types::operator_activity::{OperatorActivityKind, OperatorActivitySeverity};
    use autonoetic_types::background::UserInteractionKind;

    let temp = tempdir().expect("tempdir should create");
    let mut executor = empty_executor().0;
    let store = Arc::new(
        autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(temp.path())
            .expect("store should open"),
    );
    executor.gateway_store = Some(store.clone());
    executor.turn_counter = 7;

    let health = TrajectoryHealth::Critical {
        signals: vec![DivergenceSignal::new(
            DivergenceSignalKind::FeedbackIgnored,
            SignalSeverity::Critical,
            3.0,
            1.0,
        )
        .with_evidence("repeated output_schema violation")],
    };

    executor.emit_critical_sentinel_operator_activity(
        &store,
        "session-1",
        "root-1".to_string(),
        "turn-000007",
        &health,
    );

    // A passive operator-activity advisory was recorded.
    let activity = store
        .list_operator_activity("root-1", None, 10, None)
        .expect("list should succeed");
    assert_eq!(activity.activities.len(), 1);
    assert_eq!(activity.activities[0].kind, OperatorActivityKind::SentinelNotice);
    assert_eq!(activity.activities[0].severity, OperatorActivitySeverity::Error);
    assert!(activity.activities[0].summary.contains("Sentinel [critical]"));
    assert!(activity.activities[0].summary.contains("test-agent"));

    // No answer-demanding DivergenceSentinel UserInteraction was created.
    let interactions = store
        .get_pending_interactions_for_session("session-1")
        .expect("list interactions should succeed");
    let sentinel: Vec<_> = interactions
        .into_iter()
        .filter(|i| i.kind == UserInteractionKind::DivergenceSentinel)
        .collect();
    assert!(sentinel.is_empty(), "D.7a: Critical must not push a DivergenceSentinel UserInteraction");
}
