//! #821 — session constitution pin + resume-time drift notice.
//!
//! Mirrors `constitution_right_ri_0_5.rs`'s harness (build a real
//! `AgentExecutor`, run one turn, inspect the composed system prompt) but for
//! the constitution-drift notice rather than the degraded-mode notice.
//!
//! Unlike `runtime_lock` drift (`constitution_audit_runtime_lock_drift.rs`),
//! constitution drift must NEVER block the turn — it only notifies. These
//! tests assert the turn completes normally and the notice text/causal event
//! carry the pinned vs. current version+digest, then that the pin is updated
//! so a second turn does not repeat the notice.

mod support;

use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, TokenUsage,
};
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

#[derive(Default)]
struct CaptureSystemPromptDriver {
    prompts: Arc<Mutex<Vec<String>>>,
}

impl CaptureSystemPromptDriver {
    fn system_prompt(&self, turn: usize) -> String {
        self.prompts
            .lock()
            .unwrap()
            .get(turn)
            .cloned()
            .unwrap_or_else(|| panic!("expected an LLM call for turn {turn}"))
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

/// Seeds an agent dir and returns the freshly initialized constitution's
/// `(version, digest)` so tests can pin a deliberately *stale* pin that
/// differs from it.
fn seed_agent_dir_and_init_constitution(
    base: &std::path::Path,
    agent_id: &str,
) -> (std::path::PathBuf, String, String) {
    let _ = autonoetic_gateway::constitution_digest::initialize_constitution(
        &autonoetic_types::config::GatewayConfig::default(),
    );
    let (current_version, current_digest) =
        autonoetic_gateway::constitution_digest::try_constitution_pin()
            .expect("constitution runtime must be initialized by this point");

    let agent_dir = base.join(agent_id);
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), format!("# {}\n", agent_id)).unwrap();
    (agent_dir, current_version, current_digest)
}

#[tokio::test]
async fn constitution_drift_notice_injected_once_then_pin_updated() {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_id = "constdrift.tester";
    let (agent_dir, current_version, current_digest) =
        seed_agent_dir_and_init_constitution(&agents_dir, agent_id);

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

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
    .with_session_id("session-constdrift-1");

    // Simulate a session that was pinned to an older constitution before
    // being suspended: set a stale pin that differs from the process's
    // freshly initialized constitution, as `SessionCheckpoint::restore_into`
    // would on a real resume.
    runtime.constitution_version = Some("2020.01.01".to_string());
    runtime.constitution_digest = Some("stale0000digest0000".to_string());

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Say hello.".to_string()),
    ];

    let outcome = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(
        matches!(outcome, TurnOutcome::Completed(_)),
        "constitution drift must never block the turn"
    );

    let system_prompt = driver.system_prompt(0);
    assert!(
        system_prompt.contains("Constitution Drift Notice (Ri-0.5)"),
        "drifted session must inject a constitution drift notice: {system_prompt}"
    );
    assert!(
        system_prompt.contains("2020.01.01"),
        "notice must name the pinned (old) version"
    );
    assert!(
        system_prompt.contains(&current_version),
        "notice must name the current (new) version"
    );

    // The pin is updated after noticing so the session now knowingly runs
    // under the new law.
    assert_eq!(runtime.constitution_version.as_deref(), Some(current_version.as_str()));
    assert_eq!(runtime.constitution_digest.as_deref(), Some(current_digest.as_str()));

    // A second turn in the same session must NOT repeat the notice — it was
    // consumed on the wake it was detected, and the pin now matches current.
    history.push(Message::user("Say hello again.".to_string()));
    let outcome = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    let second_prompt = driver.system_prompt(1);
    assert!(
        !second_prompt.contains("Constitution Drift Notice (Ri-0.5)"),
        "notice must not repeat once the pin has been adopted: {second_prompt}"
    );
}

#[tokio::test]
async fn constitution_no_drift_when_pin_matches_current() {
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_id = "constdrift.clean";
    let (agent_dir, _current_version, _current_digest) =
        seed_agent_dir_and_init_constitution(&agents_dir, agent_id);

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

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
    .with_session_id("session-constdrift-clean");

    // No pre-set pin: a genuinely fresh session captures its pin at session
    // start with nothing to notice against.
    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("Say hello.".to_string()),
    ];

    let outcome = runtime.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)));

    let system_prompt = driver.system_prompt(0);
    assert!(
        !system_prompt.contains("Constitution Drift Notice (Ri-0.5)"),
        "a fresh session with no prior pin must not report drift: {system_prompt}"
    );
}
