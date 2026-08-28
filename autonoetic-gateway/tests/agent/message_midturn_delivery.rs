//! Peer messages must reach a session that is *already running*, not only one
//! that is waking up.
//!
//! `tests/agent/messaging.rs` covers addressing — who is a legal recipient, how
//! `AgentMessage` patterns are enforced, which statuses mean "nothing was
//! sent". Every one of those tests stops at the queue: the success case asserts
//! `fetch_undelivered_messages(...).len() == 1`, i.e. it asserts the message is
//! still **un**delivered. Nothing drove the lifecycle far enough to prove a
//! recipient ever ingests one.
//!
//! That gap hid a real defect. Delivery was drained exactly once, before the
//! turn loop, so it only fired for a session that was asleep at send time. A
//! session already inside the loop never revisited the drain: the row sat with
//! `delivered_at` NULL until the session finished, and a finished session never
//! wakes again, so it was stranded permanently — while `agent_message` had
//! already answered `{"ok":true,"status":"delivered","recipients_count":1}`.
//!
//! These tests pin the ingest itself, from both directions.

use autonoetic_gateway::constitution_digest::initialize_constitution;
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, TokenUsage,
    ToolCall, ToolDefinition,
};
use autonoetic_gateway::policy::PolicyEngine;
use autonoetic_gateway::runtime::active_execution_registry::NativeToolRunContext;
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::tools::{default_registry, NativeTool};
use autonoetic_gateway::scheduler::gateway_store::{AgentMessageRecord, GatewayStore};
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::config::GatewayConfig;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

use crate::support::manifest_builder::TestManifest;

const RECEIVER_SESSION: &str = "session-midturn-receiver";
const PEER_BODY: &str = "SENTINEL peer body — sent while the recipient was mid-run";

/// A no-op tool that exists purely to make the executor take another trip
/// around the turn loop: a tool call keeps the loop going where `EndTurn`
/// would break out of it.
struct KeepAliveTool;

impl NativeTool for KeepAliveTool {
    fn name(&self) -> &'static str {
        "test.keep_alive"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Test tool that forces another turn".to_string(),
            input_schema: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    fn is_available(&self, _manifest: &AgentManifest) -> bool {
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn execute(
        &self,
        _manifest: &AgentManifest,
        _policy: &PolicyEngine,
        _agent_dir: &Path,
        _gateway_dir: Option<&Path>,
        _arguments_json: &str,
        _session_id: Option<&str>,
        _turn_id: Option<&str>,
        _config: Option<&GatewayConfig>,
        _gateway_store: Option<Arc<GatewayStore>>,
        _run_context: Option<&NativeToolRunContext>,
    ) -> anyhow::Result<String> {
        Ok("{\"ok\":true}".to_string())
    }
}

/// Drives two turns and records the user-role text the model saw on each.
///
/// Turn 1 optionally enqueues a peer message *for the session that is running
/// right now* — the case a pre-loop-only drain can never serve — then returns a
/// tool call so the loop iterates. Turn 2 ends the turn.
struct TwoTurnDriver {
    store: Arc<GatewayStore>,
    calls: AtomicUsize,
    /// User-role message contents observed per LLM call.
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    /// When false, no peer message is enqueued — the control case.
    enqueue_midturn: bool,
}

impl TwoTurnDriver {
    fn new(store: Arc<GatewayStore>, enqueue_midturn: bool) -> Self {
        Self {
            store,
            calls: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
            enqueue_midturn,
        }
    }

    fn enqueue_peer_message(&self) {
        let record = AgentMessageRecord {
            message_id: "msg-midturn-1".to_string(),
            sender_session_id: "peer-session-1".to_string(),
            sender_agent_id: "peer-agent".to_string(),
            target_pattern: format!("session:{RECEIVER_SESSION}"),
            message: PEER_BODY.to_string(),
            created_at: "2026-08-28T14:25:05Z".to_string(),
            egress_label: None,
        };
        self.store.save_agent_message(&record).unwrap();
        self.store
            .insert_message_delivery(&record.message_id, RECEIVER_SESSION)
            .unwrap();
    }
}

#[async_trait::async_trait]
impl LlmDriver for TwoTurnDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let user_texts: Vec<String> = req
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .collect();
        self.seen.lock().unwrap().push(user_texts);

        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // The recipient is inside its turn loop at this exact moment.
            if self.enqueue_midturn {
                self.enqueue_peer_message();
            }
            let mut assistant = Message::assistant("working");
            let call = ToolCall {
                id: "tc-keepalive".to_string(),
                name: "test.keep_alive".to_string(),
                arguments: "{}".to_string(),
            };
            assistant.tool_calls = vec![call.clone()];
            return Ok(CompletionResponse {
                text: "working".to_string(),
                tool_calls: vec![call],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            });
        }

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
        agent: AgentIdentity {
            id: agent_id.to_string(),
            name: agent_id.to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        ..TestManifest::new().build()
    }
}

fn seed_agent_dir(base: &Path, agent_id: &str) -> std::path::PathBuf {
    let agent_dir = base.join(agent_id);
    std::fs::create_dir_all(agent_dir.join("history")).unwrap();
    std::fs::write(agent_dir.join("runtime.lock"), "dependencies: []\n").unwrap();
    std::fs::write(agent_dir.join("SKILL.md"), format!("# {agent_id}\n")).unwrap();
    agent_dir
}

struct Fixture {
    _temp: tempfile::TempDir,
    store: Arc<GatewayStore>,
    driver: Arc<TwoTurnDriver>,
    executor: AgentExecutor,
}

fn fixture(enqueue_midturn: bool) -> Fixture {
    let _ = initialize_constitution(&GatewayConfig::default());
    let temp = tempdir().unwrap();
    let agents_dir = temp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let agent_id = "coder.midturn_test";
    let agent_dir = seed_agent_dir(&agents_dir, agent_id);

    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir).unwrap();
    let store = Arc::new(GatewayStore::open(&gateway_dir).unwrap());

    let driver = Arc::new(TwoTurnDriver::new(store.clone(), enqueue_midturn));

    let mut registry = default_registry();
    registry.register(Box::new(KeepAliveTool));

    let executor = AgentExecutor::new(
        manifest(agent_id),
        "You are a test agent.".to_string(),
        driver.clone(),
        agent_dir,
        registry,
        Some(store.clone()),
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id(RECEIVER_SESSION)
    .with_config(Arc::new(GatewayConfig::default()))
    .with_initial_user_message("do the work");

    Fixture {
        _temp: temp,
        store,
        driver,
        executor,
    }
}

/// The regression: a message that lands while the recipient is mid-run is
/// ingested on its next turn, and the delivery row is cleared.
#[tokio::test]
async fn message_sent_while_recipient_is_running_is_ingested_on_the_next_turn() {
    let mut f = fixture(true);

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("do the work".to_string()),
    ];
    let outcome = f.executor.execute_with_history(&mut history).await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed(_)), "{outcome:?}");

    let seen = f.driver.seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 2,
        "driver must have been called for a second turn, got {} call(s)",
        seen.len()
    );

    // Turn 1 predates the send: the block must not be there yet.
    assert!(
        !seen[0].iter().any(|t| t.contains(PEER_BODY)),
        "turn 1 ran before the peer message existed: {:?}",
        seen[0]
    );

    // Turn 2 is the proof. Before the fix this was empty of peer traffic, and
    // stayed empty for every later turn.
    let block = seen[1]
        .iter()
        .find(|t| t.contains(PEER_BODY))
        .unwrap_or_else(|| {
            panic!(
                "peer message queued mid-turn was never ingested on the next turn: {:?}",
                seen[1]
            )
        });
    assert!(
        block.contains("[Direct Message from Agent 'peer-agent' (Session: peer-session-1)]"),
        "ingested text must carry the documented header the guidance teaches: {block}"
    );

    // Delivery is acknowledged, so it is not re-injected on every later turn.
    assert!(
        f.store
            .fetch_undelivered_messages(RECEIVER_SESSION)
            .unwrap()
            .is_empty(),
        "ingested message must be marked delivered"
    );
}

/// The drain runs every turn, so it must stay cheap and silent when there is
/// nothing pending — no phantom `[Direct Message ...]` block on an ordinary
/// multi-turn session.
#[tokio::test]
async fn a_turn_with_no_pending_messages_injects_nothing() {
    let mut f = fixture(false);

    let mut history = vec![
        Message::system("You are a test agent.".to_string()),
        Message::user("do the work".to_string()),
    ];
    f.executor.execute_with_history(&mut history).await.unwrap();

    let seen = f.driver.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "expected a second turn, got {}", seen.len());
    for (i, turn) in seen.iter().enumerate() {
        assert!(
            !turn.iter().any(|t| t.contains("[Direct Message from Agent")),
            "turn {i} injected a peer message block with nothing queued: {turn:?}"
        );
    }
}
