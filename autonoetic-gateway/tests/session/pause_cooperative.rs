//! Cooperative operator pause (`root_session.pause`): the execute loop yields
//! with `YieldReason::ManualStop` at the pre-LLM checkpoint instead of the hard
//! abort used by emergency stop. The turn parks in-place; the next message
//! resumes it (the checkpoint + history are preserved). This is the gentlest
//! operator interrupt — no work is dropped and nothing needs forking.
//!
//! Distinction under test:
//! - `request_pause` → `pre_turn_checks` yields `ManualStop`, lifecycle "paused"
//! - `clear_pause` (resume) → pre-turn checks pass again (`Ok(None)`)
//! - Emergency-stop path is untouched: still `EmergencyStop` + `stop_id`

use std::sync::Arc;

use autonoetic_gateway::llm::{CompletionRequest, CompletionResponse, LlmDriver};
use autonoetic_gateway::runtime::active_execution_registry::ActiveExecutionRegistry;
use autonoetic_gateway::runtime::checkpoint::{load_latest_checkpoint, YieldReason};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::config::GatewayConfig;
use tempfile::tempdir;

use crate::support::manifest_builder::TestManifest;

struct NoopLlm;

#[async_trait::async_trait]
impl LlmDriver for NoopLlm {
    async fn complete(&self, _request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse::text_only("{}".to_string()))
    }
}

fn write_agent(agent_dir: &std::path::Path) -> anyhow::Result<()> {
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
  id: pause.tester
  name: pause.tester
  description: test
capabilities: []
---
"#,
    )?;
    Ok(())
}

/// Fresh executor wired to a real store + the shared registry.
fn executor(
    agent_dir: std::path::PathBuf,
    gateway_dir: std::path::PathBuf,
    session_id: &str,
    store: Option<Arc<GatewayStore>>,
    registry: Arc<ActiveExecutionRegistry>,
) -> AgentExecutor {
    AgentExecutor::new(
        TestManifest::new().build(),
        "You are a test agent.".to_string(),
        Arc::new(NoopLlm),
        agent_dir,
        default_registry(),
        store,
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id(session_id.to_string())
    .with_config(Arc::new(GatewayConfig::default()))
    .with_active_executions(Some(registry))
}

/// Seed the `session_transcripts` row so `set_session_lifecycle_state` (an
/// UPDATE) has a row to flip to "paused" — mirrors the real flow where the
/// session row exists before any turn runs.
fn seed_transcript(store: &GatewayStore, session_id: &str) -> anyhow::Result<()> {
    store.upsert_session_transcript(&autonoetic_types::causal_chain::SessionTranscriptRecord {
        transcript_id: format!("tr-{session_id}"),
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        agent_id: "pause.tester".to_string(),
        revision_id: None,
        user_id: None,
        started_at: chrono::Utc::now().to_rfc3339(),
        ended_at: None,
        status: "active".to_string(),
        turn_count: 0,
        transcript_handle: None,
        excerpt: None,
        origin_node_id: None,
    })
}

async fn pre_turn(runtime: &mut AgentExecutor) -> anyhow::Result<Option<autonoetic_gateway::runtime::lifecycle::TurnOutcome>> {
    let mut history = Vec::new();
    runtime.pre_turn_checks(&mut history, "turn-pause").await
}

#[serial_test::serial]
#[tokio::test]
async fn pause_request_is_consumed_and_yields_manual_stop() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let agents_dir = tmp.path().join("agents");
    let agent_dir = agents_dir.join("pause.tester");
    write_agent(&agent_dir)?;
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let registry = ActiveExecutionRegistry::new();
    const SESSION: &str = "session-pause-yield";
    seed_transcript(&store, SESSION)?;

    // Baseline: no pause → checks pass.
    let mut runtime = executor(
        agent_dir.clone(),
        gateway_dir.clone(),
        SESSION,
        Some(store.clone()),
        registry.clone(),
    );
    let first = pre_turn(&mut runtime).await?;
    assert!(first.is_none(), "no pause requested → checks should pass");
    assert!(!registry.is_pause_pending(SESSION));

    // Request a pause, then pre-turn checks must yield ManualStop.
    registry.request_pause(SESSION, "operator paused to check something");
    assert!(registry.is_pause_pending(SESSION), "flag pending after request");

    let err = pre_turn(&mut runtime).await.expect_err("pause must yield, not return Ok");
    let msg = err.to_string();
    assert!(
        msg.contains("manual_stop") && msg.contains("paused"),
        "yield error should name manual_stop pause, got: {msg}"
    );

    // The flag was consumed atomically by the loop.
    assert!(
        !registry.is_pause_pending(SESSION),
        "pause flag consumed after the loop observed it"
    );

    // Checkpoint is ManualStop (not EmergencyStop), and lifecycle is "paused".
    let cp = load_latest_checkpoint(&GatewayConfig::default(), SESSION)?.expect("checkpoint");
    assert_eq!(cp.yield_reason, YieldReason::ManualStop);

    let lifecycle = store.get_session_lifecycle_state(SESSION)?;
    assert_eq!(lifecycle.as_deref(), Some("paused"));

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn resume_clears_pause_and_pre_turn_checks_pass_again() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let agents_dir = tmp.path().join("agents");
    let agent_dir = agents_dir.join("pause.tester");
    write_agent(&agent_dir)?;
    let gateway_dir = agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    let registry = ActiveExecutionRegistry::new();
    const SESSION: &str = "session-pause-resume";

    let mut runtime = executor(
        agent_dir.clone(),
        gateway_dir.clone(),
        SESSION,
        Some(store.clone()),
        registry.clone(),
    );

    registry.request_pause(SESSION, "pause");
    assert!(registry.is_pause_pending(SESSION));

    // Operator changes their mind / resumes before the loop observes it.
    registry.clear_pause(SESSION);
    assert!(!registry.is_pause_pending(SESSION));

    let outcome = pre_turn(&mut runtime).await?;
    assert!(
        outcome.is_none(),
        "cleared pause → pre-turn checks pass again (session resumes)"
    );
    assert!(!registry.is_pause_pending(SESSION));

    Ok(())
}

#[serial_test::serial]
#[tokio::test]
async fn pause_state_machine_is_atomic_per_root() -> anyhow::Result<()> {
    let registry = ActiveExecutionRegistry::new();
    const A: &str = "root-a";
    const B: &str = "root-b";

    assert!(!registry.is_pause_pending(A));
    assert_eq!(registry.take_pause_request(A), None, "nothing to take first");

    registry.request_pause(A, "a");
    registry.request_pause(B, "b");

    assert!(registry.is_pause_pending(A));
    assert!(registry.is_pause_pending(B));

    // take consumes only the targeted root.
    let reason = registry.take_pause_request(A).expect("A pending");
    assert_eq!(reason, "a");
    assert!(!registry.is_pause_pending(A));
    assert!(registry.is_pause_pending(B), "B untouched by A's take");

    // A second take on the same root is empty.
    assert_eq!(registry.take_pause_request(A), None);

    // clear removes without consuming (idempotent on an empty root).
    registry.clear_pause(B);
    assert!(!registry.is_pause_pending(B));
    registry.clear_pause(B);
    assert!(!registry.is_pause_pending(B));

    Ok(())
}