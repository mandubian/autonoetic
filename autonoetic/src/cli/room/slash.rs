//! Slash-commands for the Session Room TUI.
//!
//! `/` enters command mode (similar to vim's `:` or Discord's `/`). Parsed
//! commands are dispatch-decisions, not strings: the room TUI matches on
//! [`SlashCommand`] variants and the parser never executes a `&str` it didn't
//! classify. The intent is to make a stray keystroke harmless — anything
//! unparseable is reported, not silently dropped.
//!
//! Currently supported:
//!
//! - `/session <id>` — switch the room to that root session id
//! - `/session list [agent]` — list recent sessions (optionally filtered by agent)
//! - `/session resume` — switch to the most recent session
//! - `/quit` / `/q` — exit the TUI
//! - `/help` / `/?` — show a one-line help summary
//!
//! Anything else returns [`SlashCommand::Unknown`]; the caller surfaces a
//! `✗ unknown command` status rather than executing.

/// Parsed slash-command. Variants carry their arguments so the dispatcher
/// doesn't re-parse strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Switch to a specific root session id (already trimmed).
    SwitchSession(String),
    /// Show a list of recent sessions, optionally filtered by agent.
    ListSessions { agent: Option<String> },
    /// Switch to the most recent session (optionally filtered by agent).
    ResumeSession { agent: Option<String> },
    /// Exit the TUI.
    Quit,
    /// Show the help text.
    Help,
    /// Anything else — the dispatcher surfaces a `✗` status.
    Unknown(String),
}

/// One-line help shown by `/help` and at the bottom of the prompt while typing.
pub const HELP_TEXT: &str = "/session <id> · /session list [agent] · /session resume [agent] · /quit · /help";

/// Parse a slash command from the user's buffer (without the leading `/`).
/// Trims surrounding whitespace; the leading `/` is expected to have been
/// consumed by the caller. Empty input returns `Unknown("")`.
pub fn parse(input: &str) -> SlashCommand {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return SlashCommand::Unknown(String::new());
    }
    // Strip exactly one leading `/` if present (caller usually already did, but
    // a doubled `//session x` shouldn't crash the parser).
    let body = trimmed.strip_prefix('/').unwrap_or(trimmed);
    // Split into head + tail. Tokens are whitespace-delimited; the head is the
    // command verb and the tail is the rest as-written (so session ids that
    // happen to contain hyphens stay intact).
    let mut parts = body.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("").to_ascii_lowercase();
    let tail = parts.next().unwrap_or("").trim();
    match head.as_str() {
        "session" => parse_session(tail),
        "quit" | "q" | "exit" => SlashCommand::Quit,
        "help" | "?" => SlashCommand::Help,
        other => SlashCommand::Unknown(other.to_string()),
    }
}

fn parse_session(tail: &str) -> SlashCommand {
    // `/session <id>` and `/session list [agent]` and `/session resume [agent]`
    // — split on whitespace to peel off the sub-verb, then take the rest as a
    // free-form session id (most ids contain hyphens, so don't tokenize further).
    // The sub-verb is matched case-insensitively, but the captured id keeps its
    // original case — session ids are not normalized.
    let trimmed_tail = tail.trim_start();
    if trimmed_tail.is_empty() {
        return SlashCommand::Unknown("session".into());
    }
    let (head_raw, rest) = match trimmed_tail.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (trimmed_tail, ""),
    };
    let sub = head_raw.to_ascii_lowercase();
    match sub.as_str() {
        "list" | "ls" => {
            let agent = (!rest.is_empty()).then(|| rest.to_string());
            SlashCommand::ListSessions { agent }
        }
        "resume" | "latest" | "last" => {
            let agent = (!rest.is_empty()).then(|| rest.to_string());
            SlashCommand::ResumeSession { agent }
        }
        // Anything else: treat the *whole* tail as a session id so the user
        // can type `/session session-abc123` without a sub-verb. The sub-verb
        // path stays for discoverability (`/session list`). Preserve the
        // original casing of the id.
        _ => {
            let id = if rest.is_empty() {
                head_raw.to_string()
            } else {
                // Both halves were non-empty — re-join so e.g.
                // `/session session-abc 123` (with a space in the id) still
                // captures the full string.
                format!("{head_raw} {rest}")
            };
            SlashCommand::SwitchSession(id.trim().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_unknown() {
        assert_eq!(parse(""), SlashCommand::Unknown(String::new()));
        assert_eq!(parse("   "), SlashCommand::Unknown(String::new()));
        assert_eq!(parse("/"), SlashCommand::Unknown(String::new()));
    }

    #[test]
    fn parse_quit_variants() {
        assert_eq!(parse("/quit"), SlashCommand::Quit);
        assert_eq!(parse("/q"), SlashCommand::Quit);
        assert_eq!(parse("/exit"), SlashCommand::Quit);
        assert_eq!(parse("  /quit  "), SlashCommand::Quit);
    }

    #[test]
    fn parse_help_variants() {
        assert_eq!(parse("/help"), SlashCommand::Help);
        assert_eq!(parse("/?"), SlashCommand::Help);
    }

    #[test]
    fn parse_session_switch_id() {
        assert_eq!(
            parse("/session session-abc123"),
            SlashCommand::SwitchSession("session-abc123".into())
        );
        assert_eq!(
            parse("/session session-deadbeef"),
            SlashCommand::SwitchSession("session-deadbeef".into())
        );
        // Hyphens, dots, and mixed case ids round-trip verbatim.
        assert_eq!(
            parse("/session My.Session_v2"),
            SlashCommand::SwitchSession("My.Session_v2".into())
        );
    }

    #[test]
    fn parse_session_list_default() {
        assert_eq!(
            parse("/session list"),
            SlashCommand::ListSessions { agent: None }
        );
        assert_eq!(
            parse("/session ls"),
            SlashCommand::ListSessions { agent: None }
        );
    }

    #[test]
    fn parse_session_list_with_agent() {
        assert_eq!(
            parse("/session list planner.default"),
            SlashCommand::ListSessions {
                agent: Some("planner.default".into())
            }
        );
    }

    #[test]
    fn parse_session_resume_default() {
        assert_eq!(
            parse("/session resume"),
            SlashCommand::ResumeSession { agent: None }
        );
        assert_eq!(
            parse("/session latest"),
            SlashCommand::ResumeSession { agent: None }
        );
    }

    #[test]
    fn parse_session_resume_with_agent() {
        assert_eq!(
            parse("/session resume planner.default"),
            SlashCommand::ResumeSession {
                agent: Some("planner.default".into())
            }
        );
    }

    #[test]
    fn unknown_verb_is_unknown() {
        assert_eq!(
            parse("/foo bar"),
            SlashCommand::Unknown("foo".into())
        );
        assert_eq!(
            parse("/nope"),
            SlashCommand::Unknown("nope".into())
        );
    }

    #[test]
    fn unknown_session_subverb_falls_back_to_id() {
        // `/session foo` (no whitespace) → SwitchSession("foo")
        assert_eq!(
            parse("/session foo"),
            SlashCommand::SwitchSession("foo".into())
        );
        // `/session foo bar` → SwitchSession("foo bar") (re-joined)
        assert_eq!(
            parse("/session foo bar"),
            SlashCommand::SwitchSession("foo bar".into())
        );
    }
}
