//! Session Room (#363 P2) — read-only renderer slice.
//!
//! `autonoetic room <root_session_id>` renders the gateway's canonical
//! `live_digest_events` timeline for a root session, oldest-first, with an
//! altitude floor and an optional `--follow` live tail. This is the first P2
//! slice: it proves timeline consumption + the channel-neutral rendering core
//! ([`render`]) before any interactive ratatui shell is built on top.

mod render;
mod tui;

use crate::cli::common::RoomArgs;
use autonoetic_types::session_timeline::Altitude;
use std::path::Path;
use std::time::Duration;

pub async fn handle_room(config_path: &Path, args: &RoomArgs) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = config.agents_dir.join(".gateway");
    let store = autonoetic_gateway::scheduler::GatewayStore::open(&gateway_dir)?;

    // `parse_str` returns `Some` for every valid floor (detail/normal/attention/
    // error), so `None` here means the input was invalid.
    let min_altitude = match Altitude::parse_str(&args.min_altitude) {
        Some(a) => a,
        None => anyhow::bail!(
            "invalid --min-altitude '{}': expected detail | normal | attention | error",
            args.min_altitude
        ),
    };

    // Interactive shell — the Session Room proper.
    if args.tui {
        return tui::run(&store, &args.root_session_id, min_altitude);
    }

    // Render the full timeline oldest-first, paging through *all* rows (not just
    // the first `limit`, which would silently truncate long sessions).
    let mut cursor: Option<String> = None;
    let rendered_any = drain_new(&store, &args.root_session_id, &mut cursor, args.limit, min_altitude)?;

    if !args.follow {
        if !rendered_any {
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
        drain_new(&store, &args.root_session_id, &mut cursor, args.limit, min_altitude)?;
    }
}

/// Render every timeline entry newer than `cursor`, paging (in `limit`-sized
/// reads) until caught up, advancing `cursor`. Returns whether anything was
/// rendered. Used for both the initial dump and each follow tick.
fn drain_new(
    store: &autonoetic_gateway::scheduler::GatewayStore,
    root_session_id: &str,
    cursor: &mut Option<String>,
    limit: u32,
    min_altitude: Altitude,
) -> anyhow::Result<bool> {
    let mut rendered_any = false;
    loop {
        let page = store.list_session_timeline(
            root_session_id,
            cursor.as_deref(),
            limit,
            Some(min_altitude),
            None,
        )?;
        if page.entries.is_empty() {
            break;
        }
        // Fold low-altitude plumbing into collapsed rows (page-local).
        for row in render::coalesce(&page.entries) {
            println!("{}", render::row_text(&row));
        }
        *cursor = page.entries.last().map(|e| e.event_id.clone());
        rendered_any = true;
        if !page.has_more {
            break;
        }
    }
    Ok(rendered_any)
}
