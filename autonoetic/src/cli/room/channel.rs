//! Channel abstraction for the Session Room (#393, P3.c).
//!
//! A *channel* is a presentation + input surface over the shared, channel-neutral
//! render core (`render.rs`). It owns **only** formatting and input affordances —
//! never merge or importance logic, which are fixed gateway-side for every
//! channel (#390). The interactive TUI is one impl, the non-interactive CLI
//! viewer another; a Discord/WhatsApp bridge (P3.d, #394) will be a third. The
//! `kind()` string is also the `channel` column value used in
//! `operator_channel_bindings` to route an external conversation back to a room.

use super::render::{self, RenderedRow};
use std::borrow::Cow;

/// A still-resolvable gate at a selection — the channel-neutral primitive every
/// surface resolves the same way (over `approvals.*` / `interaction.*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateRef {
    pub kind: GateKind,
    pub id: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GateKind {
    Approval,
    Interaction,
}

/// A resolution action a gate affords — the operator's intent, independent of
/// how any channel surfaces it (keypress, button, reaction).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GateAction {
    Approve,
    Reject,
    Answer,
}

/// A pre-digested choice for an interaction answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateOption {
    pub id: String,
    pub label: String,
}

/// A presentation + input surface over the shared render core.
pub trait Channel {
    /// Stable channel-kind identifier — also the `channel` column value in
    /// `operator_channel_bindings`. e.g. `"tui"`, `"cli"`, `"discord"`.
    fn kind(&self) -> &'static str;

    /// Format a rendered row into this channel's native line. Defaults to the
    /// channel-neutral text from [`render::row_text`]; a richer surface (styled
    /// TUI, Discord markdown) overrides and allocates as needed. Multi-line
    /// rows (those with a `detail` preview) embed `\n` and the terminal
    /// honors them.
    fn format_row<'r>(&self, row: &'r RenderedRow) -> Cow<'r, str> {
        render::row_text(row)
    }

    /// The affordance hint for resolving a pending gate in this surface (TUI
    /// keybindings here; a rich channel would surface buttons/reactions).
    fn gate_prompt(&self, gate: &GateRef) -> String;
}

/// The non-interactive CLI viewer channel: plain one-line-per-row text.
pub struct CliChannel;

impl Channel for CliChannel {
    fn kind(&self) -> &'static str {
        "cli"
    }
    fn gate_prompt(&self, gate: &GateRef) -> String {
        // The read-only viewer cannot resolve gates; point at the interactive shell.
        match gate.kind {
            GateKind::Approval => "(pending approval — resolve in `room --tui`)".into(),
            GateKind::Interaction => "(pending question — answer in `room --tui`)".into(),
        }
    }
}

/// The interactive ratatui shell channel. (Row styling is done live by the shell
/// via ratatui spans; `format_row` keeps the plain-text fallback for parity.)
pub struct TuiChannel;

impl Channel for TuiChannel {
    fn kind(&self) -> &'static str {
        "tui"
    }
    fn gate_prompt(&self, gate: &GateRef) -> String {
        match gate.kind {
            GateKind::Approval => " · ⚠ APPROVAL PENDING — y/n".into(),
            GateKind::Interaction => " · ⚠ QUESTION PENDING — Enter/i/r to answer".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_name_themselves_and_prompt_per_gate() {
        let appr = GateRef { kind: GateKind::Approval, id: "apr-1".into() };
        let ask = GateRef { kind: GateKind::Interaction, id: "int-1".into() };

        assert_eq!(TuiChannel.kind(), "tui");
        assert!(TuiChannel.gate_prompt(&appr).contains("APPROVAL"));
        assert!(TuiChannel.gate_prompt(&ask).contains("QUESTION"));
        assert!(TuiChannel.gate_prompt(&ask).contains("Enter"));

        assert_eq!(CliChannel.kind(), "cli");
        // The viewer can't resolve; it points at the interactive shell.
        assert!(CliChannel.gate_prompt(&appr).contains("--tui"));
    }
}
