//! `autonoetic session ...` subcommand. Self-Improvement loop P0 (#245).
//!
//! - `session rate <id> --thumbs-up|--thumbs-down [--note ...]` — attach
//!   an operator rating to the SessionOutcome row.
//! - `session show <id>` — print the SessionOutcome row as JSON.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::session_outcome::OperatorThumb;

use crate::cli::common::SessionCommands;

pub async fn handle_session(config_path: &Path, command: &SessionCommands) -> anyhow::Result<()> {
    let loaded_config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = loaded_config.agents_dir.join(".gateway");
    let store = Arc::new(
        GatewayStore::open(&gateway_dir)
            .context("Failed to open GatewayStore — has the gateway run at this path?")?,
    );

    match command {
        SessionCommands::Rate {
            session_id,
            thumbs_up,
            thumbs_down,
            note,
        } => handle_rate(&store, session_id, *thumbs_up, *thumbs_down, note.as_deref()),
        SessionCommands::Show { session_id } => handle_show(&store, session_id),
    }
}

fn handle_rate(
    store: &Arc<GatewayStore>,
    session_id: &str,
    thumbs_up: bool,
    thumbs_down: bool,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let thumb = match (thumbs_up, thumbs_down) {
        (true, false) => OperatorThumb::Up,
        (false, true) => OperatorThumb::Down,
        (false, false) => {
            anyhow::bail!("must specify --thumbs-up or --thumbs-down")
        }
        (true, true) => unreachable!("clap conflicts_with prevents both"),
    };

    // Soft cap on note length so operators don't accidentally paste
    // huge transcripts into the rating column. The schema column is
    // unbounded — this is a CLI-layer guard.
    if let Some(n) = note {
        if n.len() > 500 {
            anyhow::bail!(
                "--note is {} chars; cap is 500. Use a separate notes file if more detail is needed.",
                n.len()
            );
        }
    }

    store
        .set_session_outcome_operator_rating(session_id, thumb, note)
        .with_context(|| format!("failed to record operator rating for {}", session_id))?;

    println!(
        "Recorded {} rating for session `{}`",
        thumb.as_str(),
        session_id
    );
    Ok(())
}

fn handle_show(store: &Arc<GatewayStore>, session_id: &str) -> anyhow::Result<()> {
    let outcome = store
        .get_session_outcome(session_id)
        .with_context(|| format!("failed to query session_outcomes for {}", session_id))?;
    match outcome {
        Some(o) => {
            println!("{}", serde_json::to_string_pretty(&o)?);
            Ok(())
        }
        None => {
            anyhow::bail!(
                "no SessionOutcome row found for session `{}`. \
                 Rows are created automatically when a session closes; \
                 historical sessions from before P0 will not have one yet.",
                session_id
            );
        }
    }
}
