//! Regression: child-session close must NOT wipe RootSession-scoped grants.
//!
//! A RootSession grant is a workflow-level pre-authorization (plan envelope /
//! discovered envelope) that must survive child close so sibling sessions stay
//! covered. Only the root session closing (or emergency stop / explicit revoke)
//! clears grants.

use autonoetic_gateway::constitution_digest::initialize_constitution;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, LlmConfig, RuntimeDeclaration};
use autonoetic_types::background::{GrantScope, GrantTarget};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::session_outcome::SessionCloseOutcome;
use std::sync::Arc;
use tempfile::tempdir;

fn ensure_constitution() {
    let _ = initialize_constitution(&GatewayConfig::default());
}

struct EndTurnDriver;

#[async_trait::async_trait]
impl LlmDriver for EndTurnDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
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

fn manifest_with(agent_id: &str) -> AgentManifest {
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
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![],
        llm_overrides: None,
        llm_preset: None,
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
            excluded_tools: vec![],
        agentskills_import: None,
        compression: None,
        open_web: false,
        sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
    }
}

fn seed_root_grant(store: &GatewayStore, root_sid: &str, host: &str) {
    let targets = vec![GrantTarget::ExactHost(host.to_string())];
    store
        .insert_session_grant(
            root_sid,
            root_sid,
            root_sid,
            &GrantScope::RootSession,
            &targets,
            "test-envelope",
            &chrono::Utc::now().to_rfc3339(),
            None,
            None,
        )
        .unwrap();
}

fn build_child_runtime(
    agent_dir: &std::path::Path,
    gateway_dir: &std::path::Path,
    store: Arc<GatewayStore>,
    session_id: &str,
) -> AgentExecutor {
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), "# tester\n").unwrap();

    AgentExecutor::new(
        manifest_with("grant.tester"),
        "You are a test agent.".to_string(),
        Arc::new(EndTurnDriver),
        agent_dir.to_path_buf(),
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(gateway_dir.to_path_buf())
    .with_session_id(session_id)
}

async fn run_one_turn_and_close(runtime: &mut AgentExecutor, outcome: SessionCloseOutcome) {
    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("End.".to_string()),
    ];
    runtime
        .execute_with_history(&mut history)
        .await
        .expect("execute should succeed");
    runtime
        .close_session(outcome)
        .expect("close_session must not be refused");
}

#[tokio::test]
#[serial_test::serial]
async fn child_session_close_preserves_root_session_grant() {
    ensure_constitution();
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_sid = "session-root-grant";
    let host = "api.open-meteo.com";
    seed_root_grant(&store, root_sid, host);

    assert!(store.session_grants_cover_targets(root_sid, &[host.to_string()]));

    let child_dir = agents_dir.join("grant.tester");
    let mut runtime = build_child_runtime(
        &child_dir,
        &gateway_dir,
        store.clone(),
        &format!("{root_sid}/grant.tester-abc"),
    );
    run_one_turn_and_close(&mut runtime, SessionCloseOutcome::ExecuteLoopComplete).await;

    assert!(
        store.session_grants_cover_targets(root_sid, &[host.to_string()]),
        "RootSession grant must survive child-session close"
    );
    assert!(
        store.grants_cover_targets(
            &format!("{root_sid}/grant.tester-def"),
            root_sid,
            &[host.to_string()]
        ),
        "sibling child session must still be covered after another child closed"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn root_session_close_clears_grants() {
    ensure_constitution();
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_sid = "session-root-clear";
    let host = "api.open-meteo.com";
    seed_root_grant(&store, root_sid, host);

    assert!(store.session_grants_cover_targets(root_sid, &[host.to_string()]));

    let root_dir = agents_dir.join("grant.tester");
    let mut runtime =
        build_child_runtime(&root_dir, &gateway_dir, store.clone(), root_sid);
    run_one_turn_and_close(&mut runtime, SessionCloseOutcome::ExecuteLoopComplete).await;

    assert!(
        !store.session_grants_cover_targets(root_sid, &[host.to_string()]),
        "root-session close must clear grants"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn suspended_child_session_close_preserves_grants() {
    ensure_constitution();
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let root_sid = "session-root-suspended";
    let host = "api.open-meteo.com";
    seed_root_grant(&store, root_sid, host);

    let child_dir = agents_dir.join("grant.tester");
    let mut runtime = build_child_runtime(
        &child_dir,
        &gateway_dir,
        store.clone(),
        &format!("{root_sid}/grant.tester-abc"),
    );
    run_one_turn_and_close(&mut runtime, SessionCloseOutcome::ExecuteLoopSuspended).await;

    assert!(
        store.session_grants_cover_targets(root_sid, &[host.to_string()]),
        "suspended close must never clear grants (session will resume)"
    );
}
