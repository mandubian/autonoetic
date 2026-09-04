//! Terminal-workflow dispatch refusal — the `workflow_terminal` LoopGuard trap.
//!
//! Regression scenario: a root session closes with error (e.g. gateway
//! restart mid-run, GAP-1B) and its workflow is marked `Failed`. Every
//! dispatch/resume of that session used to replay the same burned turn: the
//! first `agent.spawn` deterministically rejects, the LoopGuard hard-trips
//! `workflow_terminal` (trip condition #7), the error close re-marks the
//! workflow terminal, and the next resume repeats the cycle. The dispatch
//! path now refuses *before* spending the turn and tells the operator how to
//! escape: fork from a pre-trip turn (a fork's fresh root session id gets a
//! fresh `Active` workflow on its first spawn).
//!
//! Gate semantics pinned here:
//! - `Failed`/`Cancelled` → refused, with a fork hint listing forkable turns;
//! - no forkable checkpoints → the hint suggests starting a new session;
//! - `Completed` → passes (root-planner spawn reactivates it);
//! - `EmergencyStopped` → passes (the P-6.14 refusal upstream owns that case
//!   with its own machine-matched operator contract).

use autonoetic_gateway::llm::Message;
use autonoetic_gateway::runtime::checkpoint::{save_checkpoint, SessionCheckpoint, YieldReason};
use autonoetic_gateway::runtime::guard::LoopGuard;
use autonoetic_gateway::scheduler::workflow_store::{
    ensure_workflow_for_root_session, load_workflow_run, save_workflow_run,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::GatewayExecutionService;
use autonoetic_types::workflow::WorkflowRunStatus;
use std::sync::Arc;

fn hibernation_checkpoint(session_id: &str, turn: u64) -> SessionCheckpoint {
    SessionCheckpoint {
        egress_labels: Default::default(),
        egress_ask: None,
        history: vec![Message::user("hello")],
        turn_counter: turn,
        loop_guard_state: LoopGuard::default(),
        session_state: Default::default(),
        tool_tier_escalated: false,
        session_phase: Default::default(),
        discovered_tools: Default::default(),
        blocked_state_event_emitted: false,
        extended_loaded: false,
        agent_id: "test-agent".to_string(),
        session_id: session_id.to_string(),
        turn_id: format!("turn-{:04}", turn),
        workflow_id: None,
        task_id: None,
        runtime_lock_hash: None,
        constitution_version: None,
        constitution_digest: None,
        llm_config_snapshot: None,
        tool_registry_version: None,
        yield_reason: YieldReason::Hibernation,
        content_store_refs: vec![],
        created_at: chrono::Utc::now().to_rfc3339(),
        pending_tool_state: None,
        llm_rounds_consumed: 0,
        tool_invocations_consumed: 0,
        tokens_consumed: 0,
        estimated_cost_usd: 0.0,
        compression_metadata: None,
        capsule_state: None,
        assistant_message: None,
        pending_action: None,
        suspended_at: None,
        suppress_until_turn: 0,
        trajectory_last_level: None,
        feedback_events: vec![],
    }
}

/// Create the root→workflow index for `session_id` and force the run into
/// `status`, mirroring what `fail_workflow_for_root_session` (GAP-1B) or an
/// operator cancel produces.
fn seed_workflow_in_status(
    config: &autonoetic_types::config::GatewayConfig,
    store: &GatewayStore,
    session_id: &str,
    status: WorkflowRunStatus,
) -> anyhow::Result<String> {
    let wf = ensure_workflow_for_root_session(config, Some(store), session_id, None)?;
    let mut run = load_workflow_run(config, Some(store), &wf.workflow_id)?
        .ok_or_else(|| anyhow::anyhow!("workflow '{}' vanished right after creation", wf.workflow_id))?;
    run.status = status;
    save_workflow_run(config, Some(store), &run)?;
    Ok(wf.workflow_id)
}

#[tokio::test]
#[serial_test::serial]
async fn failed_workflow_dispatch_refuses_with_fork_hint() -> anyhow::Result<()> {
    use crate::support::{seed_agent_revision, EnvGuard};

    let workspace = crate::support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = Arc::new(GatewayStore::open(&config.runtime_dir)?);
    let agent_id = "twfr-agent";
    crate::support::agents::install_content_agent(
        &workspace.agents_dir.join(agent_id),
        agent_id,
    )?;
    seed_agent_revision(&store, &config, agent_id, &workspace.agents_dir.join(agent_id))?;

    let session_id = "session-twfr-failed";
    save_checkpoint(&config, &hibernation_checkpoint(session_id, 1))?;
    save_checkpoint(&config, &hibernation_checkpoint(session_id, 2))?;
    let workflow_id = seed_workflow_in_status(&config, &store, session_id, WorkflowRunStatus::Failed)?;

    // Driver construction needs LLM env vars; the refusal happens before any
    // LLM exchange, so a stub base URL is enough.
    let _base_url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", "http://127.0.0.1:9");
    let _api_key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");
    let _openai_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store.clone()));
    let err = execution
        .spawn_agent_once(
            agent_id,
            "continue?",
            session_id,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await
        .expect_err("dispatch against a Failed workflow must refuse before burning a turn");

    let msg = err.to_string();
    assert!(
        msg.contains("already terminal"),
        "error should name the terminal state: {msg}"
    );
    assert!(
        msg.contains(&workflow_id) && msg.contains("failed"),
        "error should name the workflow id and status: {msg}"
    );
    assert!(
        msg.contains("autonoetic trace fork") && msg.contains("--at-turn"),
        "error should carry the fork escape hatch: {msg}"
    );
    assert!(
        msg.contains("turn-0001") && msg.contains("turn-0002"),
        "error should list the forkable turns: {msg}"
    );

    // The refusal must be visible in the room timeline: room-sent messages
    // dispatch via `event.ingest` with `async_mode: true`, so the RPC error
    // never reaches the operator — the canonical-timeline event is the only
    // surface the room can render.
    let timeline = store.list_session_timeline(session_id, None, 50, None, None)?;
    let refused: Vec<_> = timeline
        .entries
        .iter()
        .filter(|e| e.event_type == "session.dispatch_refused")
        .collect();
    assert_eq!(
        refused.len(),
        1,
        "exactly one dispatch_refused timeline event expected, got {}: {:?}",
        refused.len(),
        timeline
            .entries
            .iter()
            .map(|e| &e.event_type)
            .collect::<Vec<_>>()
    );
    let payload = refused[0]
        .payload
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()?
        .unwrap_or_default();
    assert_eq!(
        payload.get("workflow_id").and_then(|v| v.as_str()),
        Some(workflow_id.as_str()),
        "timeline payload should name the terminal workflow"
    );
    assert_eq!(
        payload.get("workflow_status").and_then(|v| v.as_str()),
        Some("failed")
    );
    assert!(
        payload
            .get("forkable_turns")
            .and_then(|v| v.as_array())
            .is_some_and(|t| t.len() == 2),
        "timeline payload should carry the forkable turns: {payload}"
    );

    Ok(())
}

#[tokio::test]
#[serial_test::serial]
async fn cancelled_workflow_dispatch_refuses_too() -> anyhow::Result<()> {
    use crate::support::{seed_agent_revision, EnvGuard};

    let workspace = crate::support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = Arc::new(GatewayStore::open(&config.runtime_dir)?);
    let agent_id = "twfr-cancelled-agent";
    crate::support::agents::install_content_agent(
        &workspace.agents_dir.join(agent_id),
        agent_id,
    )?;
    seed_agent_revision(&store, &config, agent_id, &workspace.agents_dir.join(agent_id))?;

    let session_id = "session-twfr-cancelled";
    save_checkpoint(&config, &hibernation_checkpoint(session_id, 1))?;
    seed_workflow_in_status(&config, &store, session_id, WorkflowRunStatus::Cancelled)?;

    let _base_url = EnvGuard::set("AUTONOETIC_LLM_BASE_URL", "http://127.0.0.1:9");
    let _api_key = EnvGuard::set("AUTONOETIC_LLM_API_KEY", "test-key");
    let _openai_key = EnvGuard::set("OPENAI_API_KEY", "test-key");

    let execution = GatewayExecutionService::new(config, Some(store));
    let err = execution
        .spawn_agent_once(
            agent_id,
            "continue?",
            session_id,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            &[],
        )
        .await
        .expect_err("dispatch against a Cancelled workflow must refuse");

    let msg = err.to_string();
    assert!(
        msg.contains("already terminal") && msg.contains("cancelled"),
        "error should name the terminal state: {msg}"
    );

    Ok(())
}

/// Without any checkpoint there is nothing to fork from — the hint must say
/// so instead of pointing at an empty fork list.
#[tokio::test]
async fn no_forkable_checkpoints_hint_suggests_new_session() -> anyhow::Result<()> {
    let workspace = crate::support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = Arc::new(GatewayStore::open(&config.runtime_dir)?);

    let session_id = "session-twfr-noforks";
    seed_workflow_in_status(&config, &store, session_id, WorkflowRunStatus::Failed)?;

    let execution = GatewayExecutionService::new(config, Some(store));
    let err = execution
        .ensure_root_workflow_resumable(session_id)
        .expect_err("Failed workflow must be refused");

    let msg = err.to_string();
    assert!(
        msg.contains("No forkable checkpoints"),
        "error should suggest starting a new session when nothing is forkable: {msg}"
    );
    assert!(
        !msg.contains("autonoetic trace fork"),
        "fork command hint is useless without forkable turns: {msg}"
    );

    Ok(())
}

/// A `Completed` workflow must NOT be blocked: the root planner is allowed to
/// reactivate it for follow-up work (terminal guard in runtime/tools/agent.rs).
#[tokio::test]
async fn completed_workflow_passes_the_gate() -> anyhow::Result<()> {
    let workspace = crate::support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = Arc::new(GatewayStore::open(&config.runtime_dir)?);

    let session_id = "session-twfr-completed";
    seed_workflow_in_status(&config, &store, session_id, WorkflowRunStatus::Completed)?;

    let execution = GatewayExecutionService::new(config, Some(store));
    execution
        .ensure_root_workflow_resumable(session_id)
        .expect("Completed workflow must pass the gate (root-planner reactivation)");

    Ok(())
}

/// `EmergencyStopped` passes the gate on purpose: the P-6.14 refusal
/// (`resume_from_checkpoint` / trigger coherence) owns that case and its
/// error strings are machine-matched downstream.
#[tokio::test]
async fn emergency_stopped_workflow_passes_the_gate() -> anyhow::Result<()> {
    let workspace = crate::support::TestWorkspace::new()?;
    let config = workspace.gateway_config();
    let store = Arc::new(GatewayStore::open(&config.runtime_dir)?);

    let session_id = "session-twfr-estop";
    seed_workflow_in_status(&config, &store, session_id, WorkflowRunStatus::EmergencyStopped)?;

    let execution = GatewayExecutionService::new(config, Some(store));
    execution
        .ensure_root_workflow_resumable(session_id)
        .expect("EmergencyStopped must pass this gate (P-6.14 owns the refusal)");

    Ok(())
}
