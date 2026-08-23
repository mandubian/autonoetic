//! CLI handlers for `autonoetic recording` commands.
//!
//! Since #1119 (tranche 3) every subcommand speaks JSON-RPC to the running
//! gateway ([`crate::cli::rpc::GatewayRpc`]) via the `recording.*` methods —
//! the CLI never opens gateway.db directly.

use std::path::Path;

use autonoetic_types::recording::RecordingSession;

use crate::cli::rpc::GatewayRpc;

fn open_rpc(config_path: &Path) -> anyhow::Result<GatewayRpc> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    GatewayRpc::from_config(&config)
}

pub fn handle_recording_list(
    config_path: &Path,
    agent: Option<&str>,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    let rpc = open_rpc(config_path)?;
    let raw = rpc.call(
        "recording.list",
        serde_json::json!({ "agent": agent, "limit": limit }),
    )?;
    let sessions: Vec<RecordingSession> = serde_json::from_value(raw)?;

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
    let rpc = open_rpc(config_path)?;
    let raw = rpc.call(
        "recording.get",
        serde_json::json!({ "session_id": session_id }),
    )?;

    let session: RecordingSession = serde_json::from_value(raw["session"].clone())?;

    if json {
        // Same shape as before the RPC migration: the session object only
        // (the linked fixture set prints in the human-readable path).
        println!("{}", serde_json::to_string_pretty(&raw["session"])?);
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

    if !raw["fixture_set"].is_null() {
        let fs: autonoetic_types::recording::FixtureSet =
            serde_json::from_value(raw["fixture_set"].clone())?;
        println!();
        println!("  Fixture Set: {}", fs.fixture_set_id);
        println!("    Files:     {}", fs.fixture_file_count);
        println!("    Total:     {} bytes", fs.total_bytes);
        println!("    Digest:    {}", fs.digest);
        println!("    Hosts:     {:?}", fs.host_summary);
        println!("    Status:    {}", fs.status.as_str());
    }

    Ok(())
}

pub fn handle_recording_delete(
    config_path: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    let rpc = open_rpc(config_path)?;
    rpc.call(
        "recording.delete",
        serde_json::json!({ "session_id": session_id }),
    )?;
    eprintln!("Recording session '{}' deleted.", session_id);
    Ok(())
}

pub fn handle_recording_cancel(
    config_path: &Path,
    session_id: &str,
) -> anyhow::Result<()> {
    let rpc = open_rpc(config_path)?;
    rpc.call(
        "recording.cancel",
        serde_json::json!({ "session_id": session_id }),
    )?;
    eprintln!("Recording session '{}' cancelled.", session_id);
    Ok(())
}
