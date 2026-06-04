//! Session Room (#363) — read-only viewer + interactive shell.
//!
//! P3.b (#392): the room is a *gateway API client*, not a direct store reader.
//! The non-interactive viewer here reads the canonical timeline over the
//! `session.timeline.list` JSON-RPC. The interactive TUI still reads the store
//! directly (its sync event loop needs an async restructure) — P3.b-2 migrates
//! it (reads via RPC, writes via `approvals.*` / `interaction.resolve_and_answer`).

mod client;
mod render;
mod tui;

use crate::cli::common::RoomArgs;
use autonoetic_types::session_timeline::{Altitude, SessionTimelineListResult};
use client::RoomClient;
use std::path::Path;
use std::time::Duration;

pub async fn handle_room(config_path: &Path, args: &RoomArgs) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;

    // `parse_str` returns `Some` for every valid floor — `None` means invalid.
    let min_altitude = match Altitude::parse_str(&args.min_altitude) {
        Some(a) => a,
        None => anyhow::bail!(
            "invalid --min-altitude '{}': expected detail | normal | attention | error",
            args.min_altitude
        ),
    };

    // Interactive shell — still a direct store reader for now; P3.b-2 migrates
    // it to the RPC client (its sync event loop needs an async restructure).
    if args.tui {
        let gateway_dir = config.agents_dir.join(".gateway");
        let store = autonoetic_gateway::scheduler::GatewayStore::open(&gateway_dir)?;
        return tui::run(&config, &store, &args.root_session_id, min_altitude, args.limit);
    }

    // Read-only viewer: a pure gateway API client (#392) — no store access.
    let client = RoomClient::from_config(&config)?;
    let mut cursor: Option<String> = None;
    let rendered_any = drain_new_rpc(
        &client,
        &args.root_session_id,
        &mut cursor,
        args.limit,
        &args.min_altitude,
    )
    .await?;

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
        "Following room '{}' (floor: {}) via gateway API. Press Ctrl+C to stop.",
        args.root_session_id, args.min_altitude
    );
    let mut interval = tokio::time::interval(Duration::from_millis(800));
    loop {
        interval.tick().await;
        drain_new_rpc(
            &client,
            &args.root_session_id,
            &mut cursor,
            args.limit,
            &args.min_altitude,
        )
        .await?;
    }
}

/// Render every timeline entry newer than `cursor` via `session.timeline.list`,
/// paging until caught up and advancing `cursor`. Returns whether anything was
/// rendered.
async fn drain_new_rpc(
    client: &RoomClient,
    root_session_id: &str,
    cursor: &mut Option<String>,
    limit: u32,
    min_altitude: &str,
) -> anyhow::Result<bool> {
    let mut rendered_any = false;
    loop {
        let result = client
            .call(
                "session.timeline.list",
                serde_json::json!({
                    "root_session_id": root_session_id,
                    "after_event_id": cursor.clone(),
                    "limit": limit,
                    "min_altitude": min_altitude,
                }),
            )
            .await?;
        let page: SessionTimelineListResult = serde_json::from_value(result)?;
        if page.entries.is_empty() {
            break;
        }
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
