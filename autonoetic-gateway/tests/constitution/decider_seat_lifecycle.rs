//! #1199: a decider seat dies with the run it was appointed over — and only
//! when that run really ends.
//!
//! Driven through `close_session`, the real path, rather than through the
//! revocation helper. The helper has its own tests; the question here is
//! whether close calls it, and whether it correctly does *not* on a
//! suspension.
//!
//! The suspension case is the trap. `close_session` fires on every yield —
//! budget exhausted, max turns, manual stop — not only on a real end. Treating
//! a yield as an end is a mistake this codebase has made before: doing it to
//! session grants destroyed operator-approved grants the resume needed. A seat
//! vacated on a yield would be the same bug wearing different clothes, so the
//! revocation sits inside the existing `!is_suspended()` guard and this suite
//! pins both directions.

use std::sync::Arc;

use autonoetic_gateway::llm::{CompletionRequest, CompletionResponse, LlmDriver};
use autonoetic_gateway::runtime::lifecycle::AgentExecutor;
use autonoetic_gateway::runtime::tools::default_registry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::ApprovalRisk;
use autonoetic_types::decider_appointment::DeciderAppointment;
use autonoetic_types::session_outcome::SessionCloseOutcome;

use crate::gate_decider::agent_manifest;

/// Never called — these tests close a session without executing a turn.
struct SilentDriver;

#[async_trait::async_trait]
impl LlmDriver for SilentDriver {
    async fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        anyhow::bail!("the lifecycle tests never run a turn")
    }
}

fn seat(id: &str, scope: &str) -> DeciderAppointment {
    DeciderAppointment {
        appointment_id: id.to_string(),
        decider_agent: "nightwatch.default".to_string(),
        decider_revision: "rev-test".to_string(),
        decider_provider: Some("anthropic".to_string()),
        decider_model: Some("claude-opus-4-20250514".to_string()),
        kinds: vec!["approval".to_string()],
        scope_root_session: scope.to_string(),
        decider_session: Some(format!("decider-{id}")),
        risk_ceiling: ApprovalRisk::High,
        advice_only: true,
        expires_at: None,
        max_gates: None,
        gates_decided: 0,
        appointed_by: "operator".to_string(),
        appointed_at: chrono::Utc::now().to_rfc3339(),
        revoked_at: None,
        revoked_by: None,
        revoked_reason: None,
    }
}

/// Close `session_id` with `outcome`, having seated `apt` over `scope`.
/// Returns whether the seat is still active afterwards.
fn seat_survives_close(
    session_id: &str,
    scope: &str,
    outcome: SessionCloseOutcome,
) -> anyhow::Result<bool> {
    let temp = tempfile::tempdir()?;
    let agent_dir = temp.path().join("agent");
    let gateway_dir = temp.path().join(".gateway");
    std::fs::create_dir_all(&agent_dir)?;
    std::fs::create_dir_all(&gateway_dir)?;

    let store = Arc::new(GatewayStore::open(&gateway_dir)?);
    store.insert_decider_appointment(&seat("apt-1", scope))?;

    let mut runtime = AgentExecutor::new(
        agent_manifest("coder.default", vec![]),
        "test".to_string(),
        Arc::new(SilentDriver),
        agent_dir,
        default_registry(),
        Some(store.clone()),
    )
    .with_gateway_dir(gateway_dir)
    .with_session_id(session_id);

    // `close_session` returns immediately unless a session actually started.
    // Without this the whole suite passes vacuously — the first draft of these
    // tests did exactly that, and only the two *negative* assertions caught it.
    runtime.session_started = true;

    runtime.close_session(outcome)?;

    let still_active = !store
        .list_decider_appointments_for_scope(scope, true)?
        .is_empty();
    Ok(still_active)
}

#[test]
fn a_run_that_really_ends_vacates_its_seat() -> anyhow::Result<()> {
    assert!(
        !seat_survives_close("root-a", "root-a", SessionCloseOutcome::ExecuteLoopComplete)?,
        "a completed run must not leave a live seat behind"
    );
    Ok(())
}

#[test]
fn a_failed_run_vacates_its_seat_too() -> anyhow::Result<()> {
    assert!(
        !seat_survives_close("root-b", "root-b", SessionCloseOutcome::SpawnExecuteError)?,
        "a run that failed is still a run that ended"
    );
    Ok(())
}

#[test]
fn a_suspended_run_keeps_its_seat() -> anyhow::Result<()> {
    // The trap. A yield is not an end: the run resumes, and it must resume with
    // the seat the operator appointed. Vacating here would be the session-grant
    // bug repeated.
    assert!(
        seat_survives_close("root-c", "root-c", SessionCloseOutcome::ExecuteLoopSuspended)?,
        "a suspended run resumes and must keep its seat"
    );
    Ok(())
}

#[test]
fn a_child_session_closing_does_not_vacate_the_runs_seat() -> anyhow::Result<()> {
    // Seats are scoped to the root. A child finishing is an ordinary event in a
    // run that is still going.
    assert!(
        seat_survives_close("root-d/child", "root-d", SessionCloseOutcome::ExecuteLoopComplete)?,
        "only the root session ending ends the run"
    );
    Ok(())
}
