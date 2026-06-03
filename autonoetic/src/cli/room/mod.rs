//! Session Room (#363 P2) — read-only renderer slice.
//!
//! `autonoetic room <root_session_id>` renders the gateway's canonical
//! `live_digest_events` timeline for a root session, oldest-first, with an
//! altitude floor and an optional `--follow` live tail. This is the first P2
//! slice: it proves timeline consumption + the channel-neutral rendering core
//! ([`render`]) before any interactive ratatui shell is built on top.

mod render;

use crate::cli::common::RoomArgs;
use autonoetic_types::session_timeline::Altitude;
use std::path::Path;
use std::time::Duration;

pub async fn handle_room(config_path: &Path, args: &RoomArgs) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = config.agents_dir.join(".gateway");
    let store = autonoetic_gateway::scheduler::GatewayStore::open(&gateway_dir)?;

    let min_altitude = Altitude::parse_str(&args.min_altitude);
    if min_altitude.is_none() && args.min_altitude != "detail" {
        anyhow::bail!(
            "invalid --min-altitude '{}': expected detail | normal | attention | error",
            args.min_altitude
        );
    }

    // Initial window: most recent `limit` rows at/above the floor.
    let first = store.list_session_timeline(
        &args.root_session_id,
        None,
        args.limit,
        min_altitude,
        None,
    )?;
    for entry in &first.entries {
        println!("{}", render::render_line(entry));
    }
    let mut cursor = first.entries.last().map(|e| e.event_id.clone());

    if !args.follow {
        if first.entries.is_empty() {
            eprintln!(
                "(no activity at or above '{}' for session '{}')",
                args.min_altitude, args.root_session_id
            );
        }
        return Ok(());
    }

    eprintln!(
        "Following room '{}' (floor: {}). Press Ctrl+C to stop.",
        args.root_session_id, args.min_altitude
    );
    let mut interval = tokio::time::interval(Duration::from_millis(800));
    loop {
        interval.tick().await;
        // Drain everything newer than the cursor (page until caught up).
        loop {
            let page = store.list_session_timeline(
                &args.root_session_id,
                cursor.as_deref(),
                args.limit,
                min_altitude,
                None,
            )?;
            if page.entries.is_empty() {
                break;
            }
            for entry in &page.entries {
                println!("{}", render::render_line(entry));
                cursor = Some(entry.event_id.clone());
            }
            if !page.has_more {
                break;
            }
        }
    }
}
