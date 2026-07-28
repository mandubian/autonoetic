//! Issue #768 (citizenship RFC Part B.1) — injected recall at wake must be
//! task-matched, not merely the N most recent memories, and must carry
//! provenance so the agent can weigh it.

use autonoetic_gateway::constitution_digest::initialize_constitution;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::memory::{MemoryObject, MemorySourceType, MemoryVisibility};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

fn ensure_constitution() {
    let _ = initialize_constitution(&GatewayConfig::default());
}

#[derive(Default)]
struct CaptureSystemPromptDriver {
    prompts: Arc<Mutex<Vec<String>>>,
}

impl CaptureSystemPromptDriver {
    fn first_system_prompt(&self) -> String {
        self.prompts
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("expected at least one LLM call")
    }
}

#[async_trait::async_trait]
impl LlmDriver for CaptureSystemPromptDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let system_prompt = req
            .messages
            .iter()
            .find(|m| m.role == Role::System)
            .map(|m| m.content.clone())
            .expect("system prompt must be present");
        self.prompts.lock().unwrap().push(system_prompt);

        Ok(CompletionResponse {
            text: "done".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        })
    }
}

fn manifest(agent_id: &str) -> AgentManifest {
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

fn seed_agent_dir(base: &std::path::Path, agent_id: &str) -> std::path::PathBuf {
    let agent_dir = base.join(agent_id);
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), format!("# {}\n", agent_id)).unwrap();
    agent_dir
}

fn seed_digest_memory(
    store: &GatewayStore,
    id: &str,
    agent_id: &str,
    scope: &str,
    content: &str,
    session: &str,
    created_at: &str,
) {
    let mut mem = MemoryObject::new(
        id.to_string(),
        scope.to_string(),
        agent_id.to_string(),
        agent_id.to_string(),
        format!("session:{session}:post_digest"),
        content.to_string(),
    );
    mem.source_type = MemorySourceType::SessionDigest;
    mem.tags = vec![
        "source:post_session_digest".to_string(),
        format!("session:{session}"),
        format!("agent:{agent_id}"),
    ];
    mem.visibility = MemoryVisibility::Global;
    mem.created_at = created_at.to_string();
    mem.updated_at = created_at.to_string();
    store.memory_upsert(&mem).unwrap();
}

#[tokio::test]
async fn wake_injects_task_matched_lesson_with_provenance_over_newer_irrelevant_one() {
    ensure_constitution();
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_id = "coder.recall_test";
    let agent_dir = seed_agent_dir(&agents_dir, agent_id);

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    // Relevant to the incoming task, but older.
    seed_digest_memory(
        &store,
        "mem-relevant",
        agent_id,
        "digest.lesson",
        "weather api requires retry on 429 rate limit responses",
        "sess-relevant-abc12345",
        "2026-01-01T00:00:00Z",
    );
    // Irrelevant, but newer — must NOT be preferred over the relevant lesson.
    seed_digest_memory(
        &store,
        "mem-irrelevant",
        agent_id,
        "digest.fact",
        "unrelated database schema migration notes",
        "sess-irrelevant-zzz999",
        "2026-06-01T00:00:00Z",
    );

    let config = Arc::new(GatewayConfig::default());
    let driver = Arc::new(CaptureSystemPromptDriver::default());
    let mut runtime = AgentExecutor::new(
        manifest(agent_id),
        "You are a test agent.".to_string(),
        driver.clone(),
        agent_dir,
        default_registry(),
        Some(store),
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id("session-recall-wake")
    .with_config(config)
    .with_initial_user_message("please fetch weather data from the api");

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("please fetch weather data from the api".to_string()),
    ];

    let outcome = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let system_prompt = driver.first_system_prompt();
    assert!(
        system_prompt.contains("Prior Knowledge (from past sessions)"),
        "expected injected memory context: {system_prompt}"
    );
    assert!(
        system_prompt.contains("weather api requires retry"),
        "expected task-relevant lesson injected: {system_prompt}"
    );
    assert!(
        system_prompt.contains("[from session sess-rel"),
        "expected provenance suffix naming the source session: {system_prompt}"
    );
    // Both memories fit within the default priming limit (5), so the
    // irrelevant one is still present — but relevance ranking (not recency)
    // must place the task-matched lesson first.
    let relevant_pos = system_prompt
        .find("weather api requires retry")
        .expect("relevant lesson must be present");
    let irrelevant_pos = system_prompt
        .find("database schema migration")
        .expect("irrelevant memory should still be present (limit not exhausted)");
    assert!(
        relevant_pos < irrelevant_pos,
        "task-relevant lesson must be ranked before the newer but irrelevant memory: {system_prompt}"
    );
}
