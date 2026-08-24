//! Integration: live `digest.md` narrative (turns, annotations, session summary).


use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage, ToolCall,
};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig};
use autonoetic_types::session_outcome::SessionCloseOutcome;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

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
                    arguments: r#"{"type":"observation","content":"User prefers short answers."}"#
                        .to_string(),
                }],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            })
        } else {
            Ok(CompletionResponse {
                text: "Done.".to_string(),
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
        agent: AgentIdentity {
            id: "digest.tester".to_string(),
            name: "digest.tester".to_string(),
            description: "live digest integration".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
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
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        }),
        ..TestManifest::new().build()
    }
}

#[tokio::test]
async fn live_digest_records_annotation_turns_and_summary() -> anyhow::Result<()> {
    // The reply path reads the constitution version for the state-attestation
    // tail (P-6.23). A config-mismatch error only means a neighbour test
    // initialized first — that runtime satisfies the read.
    if let Err(e) = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    ) {
        anyhow::ensure!(
            autonoetic_gateway::constitution_digest::is_constitution_initialized(),
            "constitution runtime failed to initialize and no neighbour initialized it either: {e}"
        );
    }
    let temp = tempdir()?;
    let agents_dir = temp.path().join("agents");
    let agent_dir = agents_dir.join("digest.tester");
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
  id: digest.tester
  name: digest.tester
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
    .with_session_id("session-live-digest-a");

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Say hello.".to_string()),
    ];

    let _ = runtime
        .execute_with_history(&mut history)
        .await
        .expect("execute should succeed");
    runtime.close_session(SessionCloseOutcome::ExecuteLoopComplete)?;

    let digest_path = gateway_dir
        .join("sessions")
        .join("session-live-digest-a")
        .join("digest.md");
    let digest = std::fs::read_to_string(&digest_path)?;
    assert!(
        digest.contains("# Live session digest"),
        "digest missing header"
    );
    assert!(
        digest.contains("👀 **Observation:**") && digest.contains("User prefers short answers"),
        "annotation missing: {digest}"
    );
    assert!(digest.contains("**Turn 1**"), "expected turn headings");
    assert!(
        digest.contains("Session summary"),
        "expected session summary: {digest}"
    );
    assert!(
        digest.contains("execute_loop_complete"),
        "expected close reason in summary"
    );

    Ok(())
}
