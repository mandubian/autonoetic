//! Integration: structured live/session reports are written beside `digest.md`.

use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage, ToolCall,
};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig, RuntimeDeclaration};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;

struct AnnotateThenStopDriver {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmDriver for AnnotateThenStopDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(CompletionResponse {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "tc-annotate".to_string(),
                    name: "digest_annotate".to_string(),
                    arguments:
                        r#"{"type":"observation","content":"Track structured session reporting."}"#
                            .to_string(),
                }],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })
        } else {
            Ok(CompletionResponse {
                text: "Structured reporting complete.".to_string(),
                tool_calls: vec![],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
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
            id: "report.tester".to_string(),
            name: "report.tester".to_string(),
            description: "session report integration".to_string(),
        },
        capabilities: vec![],
        llm_config: Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            temperature: 0.0,
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
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

#[tokio::test]
async fn session_report_writes_live_and_final_files() -> anyhow::Result<()> {
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("report.tester");
    std::fs::create_dir_all(agent_dir.join("history"))?;
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n")?;
    std::fs::write(
        agent_dir.join("SKILL.md"),
        r#"---
version: "1.0"
runtime:
  engine: autonoetic
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: stateful
  sandbox: bubblewrap
  runtime_lock: runtime.lock
agent:
  id: report.tester
  name: report.tester
  description: test
capabilities: []
---
"#,
    )?;

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store =
        Arc::new(autonoetic_gateway::scheduler::gateway_store::GatewayStore::open(&gateway_dir)?);
    let driver = Arc::new(AnnotateThenStopDriver {
        calls: AtomicUsize::new(0),
    });

    let mut runtime = AgentExecutor::new(
        test_manifest(),
        "You are a test agent.".to_string(),
        driver,
        agent_dir.clone(),
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(gateway_dir.clone())
    .with_session_id("session-report-a");

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Say hello.".to_string()),
    ];

    let _ = runtime.execute_with_history(&mut history).await?;
    runtime.close_session("structured session report integration complete")?;

    let session_dir = gateway_dir.join("sessions").join("session-report-a");
    let live = std::fs::read_to_string(session_dir.join("session_overview.md"))?;
    let final_md = std::fs::read_to_string(session_dir.join("session_report.md"))?;
    let final_json = std::fs::read_to_string(session_dir.join("session_report.json"))?;
    let final_html = std::fs::read_to_string(session_dir.join("session_report.html"))?;

    assert!(live.contains("Session overview"));
    assert!(live.contains("report.tester"));
    assert!(live.contains("Structured reporting complete."));
    assert!(final_md.contains("Agent Summary"));
    assert!(final_md.contains("report.tester"));
    assert!(final_md.contains("Sub-Agent Ledger"));
    assert!(final_json.contains("\"output_preview\": \"Structured reporting complete.\""));
    assert!(final_html.contains("Session Report"));
    assert!(final_html.contains("report.tester"));
    assert!(final_html.contains("badge"));
    assert!(final_html.contains("stat-grid"));

    Ok(())
}
