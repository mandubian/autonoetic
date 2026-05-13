//! CLI handlers for `autonoetic recording` commands.

use std::path::Path;
use std::sync::Arc;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::recording::RecordingStatus;

fn open_store(config_path: &Path) -> anyhow::Result<Arc<GatewayStore>> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = config.agents_dir.join(".gateway");
    std::fs::create_dir_all(&gateway_dir)?;
    Ok(Arc::new(GatewayStore::open(&gateway_dir)?))
}

pub fn handle_recording_list(
    config_path: &Path,
    agent: Option<&str>,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let store = open_store(config_path)?;
    let sessions = store.list_recording_sessions(agent, limit)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }

    if sessions.is_empty() {
        eprintln!("No recording sessions found.");
        return Ok(());
    }

    println!("{:<40} {:<20} {:<12} {:<8} {:<20}", "Session ID", "Agent", "Status", "Requests", "Started");
    println!("{}", "-".repeat(100));
    for s in &sessions {
        println!(
            "{:<40} {:<20} {:<12} {:<8} {:<20}",
            s.session_id,
            s.agent_id,
            s.status.as_str(),
            s.request_count,
            &s.started_at[..19],
        );
    }
    Ok(())
}

pub fn handle_recording_inspect(
    config_path: &Path,
    session_id: &str,
    json: bool,
) -> anyhow::Result<()> {
    let store = open_store(config_path)?;

    let session = store
        .get_recording_session(session_id)?
        .ok_or_else(|| anyhow::anyhow!("Recording session '{}' not found", session_id))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&session)?);
        return Ok(());
    }

    println!("Recording Session: {}", session.session_id);
    println!("  Agent:            {}", session.agent_id);
    println!("  Status:           {}", session.status.as_str());
    println!("  Started:          {}", session.started_at);
    println!("  Stopped:          {}", session.stopped_at.unwrap_or_default());
    println!("  Duration (max):   {:?}", session.duration_secs);
    println!("  Max requests:     {:?}", session.max_requests);
    println!("  Max bytes:        {:?}", session.max_bytes);
    println!("  Request count:    {}", session.request_count);
    println!("  Total bytes:      {}", session.total_bytes);
    println!("  Fixture set:      {}", session.fixture_set_id.as_deref().unwrap_or("none"));

    if let Some(ref fs_id) = session.fixture_set_id {
        if let Ok(Some(fs)) = store.get_fixture_set(fs_id) {
            println!();
            println!("  Fixture Set: {}", fs.fixture_set_id);
            println!("    Files:     {}", fs.fixture_file_count);
            println!("    Total:     {} bytes", fs.total_bytes);
            println!("    Digest:    {}", fs.digest);
            println!("    Hosts:     {:?}", fs.host_summary);
            println!("    Status:    {}", fs.status.as_str());
        }
    }

    Ok(())
}

pub fn handle_recording_delete(
    config_path: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    let store = open_store(config_path)?;

    let session = store
        .get_recording_session(session_id)?
        .ok_or_else(|| anyhow::anyhow!("Recording session '{}' not found", session_id))?;

    // Delete the fixture set if linked.
    if let Some(ref fs_id) = session.fixture_set_id {
        store.delete_fixture_set(fs_id)?;
    }

    store.delete_recording_session(session_id)?;
    eprintln!("Recording session '{}' deleted.", session_id);
    Ok(())
}

pub fn handle_recording_cancel(
    config_path: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    let store = open_store(config_path)?;
    store.stop_recording_session(session_id, RecordingStatus::Cancelled)?;

    let causal_event = autonoetic_types::causal_chain::CausalEventRecord {
        event_id: uuid::Uuid::new_v4().to_string(),
        agent_id: String::new(),
        session_id: session_id.to_string(),
        turn_id: None,
        event_seq: chrono::Utc::now().timestamp_millis().max(0) as u64,
        timestamp: chrono::Utc::now().to_rfc3339(),
        category: "artifact".to_string(),
        action: "artifact.fixture_recording_cancelled".to_string(),
        status: "cancelled".to_string(),
        enforced_rules: vec![],
        target: Some(session_id.to_string()),
        payload: None,
        payload_ref: None,
        evidence_ref: None,
        reason: Some("Operator cancelled via CLI".to_string()),
    };
    let _ = store.create_causal_event(&causal_event);

    eprintln!("Recording session '{}' cancelled.", session_id);
    Ok(())
}
