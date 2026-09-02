//! Session Room (#363) — read-only viewer + interactive shell.
//!
//! P3.b (#392): the room is a *gateway API client*, not a direct store reader
//! (Separation of Powers). Both the viewer and the interactive TUI read the
//! canonical timeline over `session.timeline.list` and resolve gates over
//! `approvals.approve`/`reject` + `interaction.resolve_and_answer` — no
//! direct gateway-store access (enforced by `tests/cli_store_boundary.rs`).

mod channel;
pub(crate) mod client;
mod markdown;
mod render;
mod slash;
mod test_scenarios;
mod tui;

use crate::cli::common::RoomArgs;
use autonoetic_types::session_timeline::{
    Altitude, SessionTimelineEntry, SessionTimelineListResult,
};
use channel::{Channel, CliChannel};
use client::RoomClient;
use std::path::Path;
use std::time::Duration;

pub async fn handle_room(config_path: &Path, args: &RoomArgs) -> anyhow::Result<()> {
    handle_room_with_target(config_path, args, None).await
}

pub async fn handle_room_with_target(
    config_path: &Path,
    args: &RoomArgs,
    mut target_agent_id: Option<String>,
) -> anyhow::Result<()> {
    let config = autonoetic_gateway::config::load_config(config_path)?;

    // Operator tier overrides must be valid — an unnoticed typo would silently
    // fall back to the built-in classification and look like a no-op.
    for (event_type, tier) in &config.session_room.event_tiers {
        if render::TierSetting::parse(tier).is_none() {
            anyhow::bail!(
                "invalid session_room.event_tiers value for '{event_type}': '{tier}' \
                 (expected checkpoint | significant | routine | hidden)"
            );
        }
    }

    // `story` is a pure view preset (narrative + gates + failures) — the
    // gateway only understands real altitudes, so the RPC floor fetches
    // everything (`detail`) and the story admission filter is applied
    // client-side in both the TUI and the read-only viewer/follow paths.
    // `None` (flag absent) lets the TUI fall back to the persisted view
    // prefs; the non-interactive viewer then defaults to `detail`.
    let cli_floor = match args.min_altitude.as_deref() {
        Some("story") | Some("Story") | Some("STORY") => Some(render::FloorMode::Story),
        Some(s) => match Altitude::parse_str(s) {
            Some(a) => Some(render::FloorMode::Altitude(a)),
            None => anyhow::bail!(
                "invalid --min-altitude '{}': expected detail | normal | attention | error | story",
                s
            ),
        },
        None => None,
    };
    let story = matches!(cli_floor, Some(render::FloorMode::Story));
    let rpc_floor = if story { "detail" } else {
        args.min_altitude.as_deref().unwrap_or("detail")
    };

    // The whole room is a gateway API client (#392) — no store access.
    let client = RoomClient::from_config(&config)?;

    // Resolve the session id before any read. `--resume` asks the gateway for
    // the most recent session (optionally filtered by --agent); otherwise the
    // caller must supply a positional `root_session_id`.
    let mut root_session_id = resolve_root_session_id(&client, args).await?;

    // Interactive shell — reads via session.timeline.list, resolves gates via
    // approvals.* / interaction.resolve_and_answer.
    if args.tui {
        let presets: Vec<String> = config.llm_presets.keys().cloned().collect();
        return tui::run(
            &client,
            &mut root_session_id,
            cli_floor,
            args.limit,
            &mut target_agent_id,
            &presets,
            &config.session_room.event_tiers,
        );
    }

    // Read-only viewer.
    let mut cursor: Option<String> = None;
    // The viewer is non-interactive: explicit floor or `detail`, no prefs.
    let viewer_floor = if story {
        render::FloorMode::Story
    } else {
        render::FloorMode::Altitude(Altitude::parse_str(rpc_floor).unwrap_or(Altitude::Detail))
    };
    let rendered_any = drain_new_rpc(
        &client,
        &root_session_id,
        &mut cursor,
        args.limit,
        rpc_floor,
        viewer_floor,
        &config.session_room.event_tiers,
    )
    .await?;

    if !args.follow {
        if !rendered_any {
            eprintln!(
                "(no activity at or above '{}' for session '{}')",
                rpc_floor, root_session_id
            );
        }
        return Ok(());
    }

    eprintln!(
        "Following room '{}' (floor: {}) via the {} channel. Press Ctrl+C to stop.",
        root_session_id,
        rpc_floor,
        CliChannel.kind(),
    );
    let mut interval = tokio::time::interval(Duration::from_millis(800));
    loop {
        interval.tick().await;
        drain_new_rpc(
            &client,
            &root_session_id,
            &mut cursor,
            args.limit,
            rpc_floor,
            viewer_floor,
            &config.session_room.event_tiers,
        )
        .await?;
    }
}

/// Pick the `root_session_id` to render, based on the args. Explicit positional
/// always wins; `--resume` is the fallback for the standalone `autonoetic room`
/// launch (no chat loop, no fresh session_id).
async fn resolve_root_session_id(
    client: &RoomClient,
    args: &RoomArgs,
) -> anyhow::Result<String> {
    if let Some(id) = args.root_session_id.as_deref() {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    if !args.resume {
        anyhow::bail!(
            "no <SESSION_ID> given and --resume is not set. Pass a root session id \
             (e.g. `autonoetic room session-abc123`) or add --resume to pick the \
             most recent one (`autonoetic room --resume [--agent <id>]`)."
        );
    }
    let params = serde_json::json!({
        "agent_id": args.agent,
        "limit": 1,
    });
    let value = client.call("session.list", params).await?;
    let parsed: autonoetic_types::session_timeline::SessionListResult =
        serde_json::from_value(value).map_err(|e| {
            anyhow::anyhow!("malformed session.list response: {e}")
        })?;
    let entry = parsed.sessions.into_iter().next().ok_or_else(|| {
        let agent_hint = args
            .agent
            .as_deref()
            .map(|a| format!(" for agent '{a}'"))
            .unwrap_or_default();
        anyhow::anyhow!(
            "no sessions found in the gateway{agent_hint}. Start one with `autonoetic run` \
             or pass an explicit <SESSION_ID>."
        )
    })?;
    eprintln!(
        "  Resolved --resume → session {} (agent: {}, last activity: {})",
        entry.root_session_id, entry.agent_id, entry.last_active_at
    );
    Ok(entry.root_session_id)
}

/// Render every timeline entry newer than `cursor` via `session.timeline.list`,
/// paging until caught up and advancing `cursor`. Returns whether anything was
/// rendered.
///
/// `rpc_floor` is the gateway-side altitude floor — always a real altitude
/// (`story` resolves to `detail` upstream; the story admission filter runs
/// here via `view_floor`). Pure-view filtering (story preset + operator tier
/// overrides) is applied client-side before coalescing.
async fn drain_new_rpc(
    client: &RoomClient,
    root_session_id: &str,
    cursor: &mut Option<String>,
    limit: u32,
    rpc_floor: &str,
    view_floor: render::FloorMode,
    tier_overrides: &std::collections::HashMap<String, String>,
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
                    "min_altitude": rpc_floor,
                }),
            )
            .await?;
        let page: SessionTimelineListResult = serde_json::from_value(result)?;
        if page.entries.is_empty() {
            break;
        }
        let admitted: Vec<SessionTimelineEntry> = page
            .entries
            .iter()
            .filter(|e| view_floor.admits(e) && !render::is_hidden_by_config(e, tier_overrides))
            .cloned()
            .collect();
        for row in render::coalesce_with(&admitted, tier_overrides) {
            println!("{}", CliChannel.format_row(&row));
        }
        *cursor = page.entries.last().map(|e| e.event_id.clone());
        rendered_any = true;
        if !page.has_more {
            break;
        }
    }
    Ok(rendered_any)
}
