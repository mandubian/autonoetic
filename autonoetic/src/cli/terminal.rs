//! Shared terminal helpers for interactive TUIs.

use std::io::IsTerminal;

/// Fail fast with a clear message when stdout/stdin are not a TTY.
pub fn require_interactive_terminal(command: &str) -> anyhow::Result<()> {
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        return Ok(());
    }
    anyhow::bail!(
        "{command} requires an interactive terminal (TTY).\n\
         Run it from a real terminal — not a pipe, background task, or non-interactive IDE panel.\n\
         Headless alternative: start the gateway (`autonoetic gateway start`) and use \
         `autonoetic room <SESSION_ID>` without `--tui` to tail the timeline."
    )
}
