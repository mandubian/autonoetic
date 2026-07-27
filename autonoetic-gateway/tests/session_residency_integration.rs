//! Integration test: resident sessions (`agent.resident_idle_ttl_secs`).
//!
//! A resident agent parks in `YieldReason::Idle` when its task finishes instead
//! of terminating, so peers can still reach it with `agent_message`. These tests
//! cover the property that makes the feature real — a parked session is an
//! addressable recipient, and a reaped one is not — plus the invariant that
//! parking must never leave a session advertised but unreachable.

mod support;

use std::sync::Arc;

use autonoetic_gateway::scheduler::gateway_store::{GatewayStore, SessionResidency};
use autonoetic_types::agent_revision::SessionAgentBinding;

fn store() -> anyhow::Result<(support::TestWorkspace, Arc<GatewayStore>)> {
    let workspace = support::TestWorkspace::new()?;
    let gateway_dir = workspace.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    Ok((workspace, Arc::new(GatewayStore::open(&gateway_dir)?)))
}

fn bind(store: &GatewayStore, session_id: &str, agent_id: &str) -> anyhow::Result<()> {
    store.upsert_session_agent_binding(&SessionAgentBinding {
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        alias_id: None,
        agent_id: agent_id.to_string(),
        revision_id: "rev-1".to_string(),
        runtime_lock_hash: "hash".to_string(),
        home_node_id: "node".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        requested_target: agent_id.to_string(),
        constitution_version: None,
        constitution_digest: None,
    })?;
    Ok(())
}

fn park(
    store: &GatewayStore,
    session_id: &str,
    agent_id: &str,
    expires_at: &str,
) -> anyhow::Result<()> {
    store.upsert_session_residency(&SessionResidency {
        session_id: session_id.to_string(),
        root_session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        turn_id: "turn-000003".to_string(),
        since: "2026-01-01T00:00:00Z".to_string(),
        expires_at: expires_at.to_string(),
    })?;
    Ok(())
}

/// Mark a session closed the way a real close does.
fn close(store: &GatewayStore, session_id: &str, agent_id: &str) -> anyhow::Result<()> {
    store.upsert_session_outcome_metrics(session_id, session_id, agent_id, None, 1, 10, 0.0, 0.0)
}

/// The property the whole feature exists for: a session that has finished its
/// task is still a recipient while parked. Without residency this session has a
/// terminal outcome row and is unreachable — which is why nothing was ever
/// addressable before.
#[serial_test::serial]
#[tokio::test]
async fn a_parked_session_is_addressable_even_though_its_task_finished() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "resident-1", "peer-agent")?;
    close(&store, "resident-1", "peer-agent")?;

    assert!(
        store
            .list_unfinished_sessions_for_agent("peer-agent")?
            .is_empty(),
        "premise: a finished task leaves the session unreachable by the outcome-row rule"
    );

    park(&store, "resident-1", "peer-agent", "2999-01-01T00:00:00Z")?;

    assert_eq!(
        store.list_addressable_sessions_for_agent("peer-agent")?,
        vec!["resident-1".to_string()],
        "parking must make the finished session addressable again"
    );
    Ok(())
}

/// A park that has aged out stops answering before the reaper runs, so expiry
/// is enforced on read as well as by the sweep.
#[serial_test::serial]
#[tokio::test]
async fn an_expired_park_is_not_addressable() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "resident-1", "peer-agent")?;
    close(&store, "resident-1", "peer-agent")?;
    park(&store, "resident-1", "peer-agent", "2020-01-01T00:00:00Z")?;

    assert!(
        store
            .list_addressable_sessions_for_agent("peer-agent")?
            .is_empty(),
        "an elapsed TTL must stop the session answering broadcasts"
    );
    Ok(())
}

/// Deterministic expiry boundary, independent of wall clock.
#[serial_test::serial]
#[tokio::test]
async fn residency_expiry_is_evaluated_against_the_supplied_instant() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "resident-1", "peer-agent")?;
    park(&store, "resident-1", "peer-agent", "2026-06-01T00:00:00Z")?;

    assert_eq!(
        store.list_resident_sessions_for_agent_at("peer-agent", "2026-05-31T23:59:59Z")?,
        vec!["resident-1".to_string()],
        "still parked one second before expiry"
    );
    assert!(
        store
            .list_resident_sessions_for_agent_at("peer-agent", "2026-06-01T00:00:01Z")?
            .is_empty(),
        "gone one second after"
    );
    Ok(())
}

/// Re-parking after handling a message extends the TTL rather than duplicating
/// the row — otherwise a busy resident agent would age out mid-conversation.
#[serial_test::serial]
#[tokio::test]
async fn re_parking_refreshes_the_ttl_in_place() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "resident-1", "peer-agent")?;
    park(&store, "resident-1", "peer-agent", "2026-01-01T00:10:00Z")?;
    park(&store, "resident-1", "peer-agent", "2026-01-01T00:20:00Z")?;

    let rows = store.list_resident_sessions_for_agent_at("peer-agent", "2026-01-01T00:15:00Z")?;
    assert_eq!(
        rows,
        vec!["resident-1".to_string()],
        "the refreshed TTL must apply, and only one row may exist"
    );

    let r = store
        .get_session_residency("resident-1")?
        .expect("residency row");
    assert_eq!(r.expires_at, "2026-01-01T00:20:00Z");
    Ok(())
}

/// Reaping must close the session for real: write the terminal outcome row and
/// stop advertising it. A row cleared without an outcome would leave a session
/// that is neither addressable nor recorded as finished.
#[serial_test::serial]
#[tokio::test]
async fn reaping_closes_the_session_and_stops_advertising_it() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "resident-1", "peer-agent")?;
    park(&store, "resident-1", "peer-agent", "2020-01-01T00:00:00Z")?;

    assert!(
        store.get_session_outcome("resident-1")?.is_none(),
        "premise: a parked session has no outcome row"
    );

    let expired = store.list_expired_session_residencies()?;
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].session_id, "resident-1");
    assert_eq!(expired[0].agent_id, "peer-agent");

    // What the scheduler's reaper does.
    store.upsert_session_outcome_metrics(
        &expired[0].session_id,
        &expired[0].root_session_id,
        &expired[0].agent_id,
        None,
        0,
        0,
        0.0,
        0.0,
    )?;
    store.clear_session_residency(&expired[0].session_id)?;

    assert!(
        store.get_session_outcome("resident-1")?.is_some(),
        "a reaped session must be recorded as finished"
    );
    assert!(
        store
            .list_addressable_sessions_for_agent("peer-agent")?
            .is_empty(),
        "a reaped session must not answer broadcasts"
    );
    assert!(store.get_session_residency("resident-1")?.is_none());
    Ok(())
}

/// Residency is per agent: parking one role must not make another reachable.
#[serial_test::serial]
#[tokio::test]
async fn residency_does_not_leak_across_agents() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "resident-1", "peer-agent")?;
    close(&store, "resident-1", "peer-agent")?;
    park(&store, "resident-1", "peer-agent", "2999-01-01T00:00:00Z")?;

    assert!(store
        .list_addressable_sessions_for_agent("other-agent")?
        .is_empty());
    Ok(())
}

/// A non-resident agent must be unaffected: no manifest opt-in, no park, and the
/// historical "task ends, session ends" behaviour is preserved.
#[serial_test::serial]
#[tokio::test]
async fn a_non_resident_agent_has_no_residency_row() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "plain-1", "plain-agent")?;
    close(&store, "plain-1", "plain-agent")?;

    assert!(store.get_session_residency("plain-1")?.is_none());
    assert!(
        store
            .list_addressable_sessions_for_agent("plain-agent")?
            .is_empty(),
        "a finished non-resident session is not a recipient"
    );
    Ok(())
}

/// Review finding (#902): the reaper closes sessions whose park has expired, but
/// a resumed session keeps the `expires_at` it parked with. Unless resume clears
/// the row, handling a message for longer than the remaining TTL gets the session
/// a terminal `session_outcomes` row written underneath it while it is still
/// running — after which every downstream reader, including the `agent_message`
/// liveness gate, treats a live session as finished.
#[serial_test::serial]
#[tokio::test]
async fn a_resumed_session_is_no_longer_reapable() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    bind(&store, "sess-resume", "responder.default")?;
    // Parked with a TTL that has already elapsed — the state a long message
    // handler would be in by the time the reaper ticks.
    park(
        &store,
        "sess-resume",
        "responder.default",
        "2020-01-01T00:00:00Z",
    )?;
    assert_eq!(store.list_expired_session_residencies()?.len(), 1);

    // What resume now does (execution.rs, checkpoint auto-resume path).
    store.clear_session_residency("sess-resume")?;

    assert!(
        store.list_expired_session_residencies()?.is_empty(),
        "a running session must not be visible to the reaper"
    );
    // Still addressable while it runs — through the unfinished-sessions arm,
    // not residency.
    assert!(
        store
            .list_addressable_sessions_for_agent("responder.default")?
            .contains(&"sess-resume".to_string()),
        "a resumed session stays addressable while executing"
    );
    assert!(
        store.get_session_outcome("sess-resume")?.is_none(),
        "no terminal outcome row should exist for a running session"
    );
    Ok(())
}

/// Deduplication must not depend on `Vec::contains`: a resident agent with many
/// sessions pays that cost on every broadcast. Order is still residency-first.
#[serial_test::serial]
#[tokio::test]
async fn addressable_sessions_are_deduplicated_and_ordered() -> anyhow::Result<()> {
    let (_ws, store) = store()?;
    for sid in ["sess-a", "sess-b"] {
        bind(&store, sid, "responder.default")?;
    }
    // sess-a is parked *and* has no outcome row, so it appears in both arms.
    park(
        &store,
        "sess-a",
        "responder.default",
        "2099-01-01T00:00:00Z",
    )?;

    let listed = store.list_addressable_sessions_for_agent("responder.default")?;
    assert_eq!(
        listed.iter().filter(|s| s.as_str() == "sess-a").count(),
        1,
        "a session in both arms must be listed once, got {listed:?}"
    );
    assert_eq!(listed.first().map(String::as_str), Some("sess-a"));
    assert!(listed.contains(&"sess-b".to_string()));
    Ok(())
}
