//! Shared terminal helpers for interactive TUIs.

use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use std::io::{IsTerminal, Write};

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

/// Undo everything an interactive TUI does to the terminal: mouse reporting,
/// bracketed paste, the alternate screen, raw mode (echo off), and a hidden
/// cursor. Idempotent — every escape sequence is a "disable/leave" that is
/// harmless when the feature was never enabled, and raw-mode-off on an already
/// normal terminal is a no-op. Safe to call more than once (both TUIs call it
/// from their `Drop` guard *and* from the signal-handler thread; the second
/// call in a row is a no-op).
pub fn restore_terminal() -> std::io::Result<()> {
    let mut out = std::io::stdout();
    execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    out.flush()?;
    let _ = disable_raw_mode();
    Ok(())
}

#[cfg(unix)]
/// Install a background thread that restores the terminal on catchable
/// termination signals — SIGTERM (`kill`), SIGHUP (terminal window closed) and
/// SIGINT delivered from *outside* the TUI (`kill -INT`) — then exits with the
/// conventional `128 + signal` status.
///
/// Why this exists: the TUIs enable mouse reporting / the alternate screen /
/// raw mode and restore them in a `Drop` guard, which runs on normal exit and
/// panic-unwind. But a *killed* process never runs destructors — SIGKILL
/// (`kill -9`) is uncatchable, and without a handler SIGTERM/SIGHUP terminate
/// immediately too. The terminal is then left in mouse-reporting mode, and
/// every subsequent mouse move is echoed as `CSI <b;x;yM` garbage (the classic
/// "crazy chars after killing a TUI" symptom).
///
/// The watcher thread uses `signal_hook::iterator`, so the restore runs in
/// normal thread context — the same crossterm calls the `Drop` guard uses,
/// with no async-signal-safety restrictions. The TUI process is a stateless
/// JSON-RPC client (the gateway owns all state), so an immediate `exit` here is
/// safe: nothing is left inconsistent beyond the terminal itself, which this
/// just repaired.
///
/// SIGKILL can never be intercepted; the escape hatch for `kill -9` is the
/// terminal emulator resetting mouse mode when the pty closes (some
/// multiplexers such as tmux keep relaying it — restarting tmux or running
/// `reset` clears the leftover state).
///
/// In-TUI Ctrl+C is unaffected: raw mode turns it into a key event (crossterm
/// reads byte `0x03`), never a SIGINT, so the TUI's own quit path — and its
/// `Drop` restore — still runs.
pub fn install_signal_terminal_restore() -> anyhow::Result<()> {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGINT])?;
    std::thread::Builder::new()
        .name("terminal-restore-signal".to_string())
        .spawn(move || {
            // First signal wins: restore the terminal, then die as if the
            // signal had its default disposition (128 + signum, the
            // convention shells report for signal-killed children).
            for sig in signals.forever() {
                let _ = restore_terminal();
                std::process::exit(128 + sig);
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_terminal_is_idempotent_and_safe_off_tty() {
        // With `cargo test -- --nocapture` from an interactive terminal the
        // test harness stdout *is* a TTY, and the restore escape sequences
        // would visibly scribble onto the developer's screen. The property we
        // guard is the off-TTY path (CI, pipelines, editors); skip otherwise.
        if std::io::stdout().is_terminal() {
            eprintln!("skipping: stdout is a TTY (--nocapture?)");
            return;
        }
        // The TUIs call this from both their Drop guard and the
        // signal-handler thread, so it must not panic and must be callable
        // twice in a row (the escape sequences and the raw-mode-off are all
        // no-ops when nothing was enabled).
        restore_terminal().expect("restore must not fail even off-tty");
        restore_terminal().expect("second restore must be a no-op, not an error");
    }
}
