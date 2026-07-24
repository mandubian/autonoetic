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
//! - `/cron` / `/cron list` — list scheduled jobs for the current session
//! - `/plan` / `/plan approve [id]` — list or approve pending PlanFrames
//! - `/return [--force] [note...]` — return the active workbench to the orchestrator
//! - `/curate [notes...]` — run memory curation on this session now (notes steer the curator)
//! - `/quit` / `/q` — exit the TUI
//! - `/help` / `/?` — full command reference in the detail pane
//! - `/model` — show current inference profile
//! - `/model <preset>` — set session-level model override
//! - `/model clear` — clear the override
//!
//! See [`help_lines()`] for the complete key map and slash-command list.

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
    /// Run a test scenario to inject synthetic events (`/test <name>`).
    Test { name: String },
    /// List scheduled cron jobs bound to the current root session.
    ListCronJobs,
    /// List PlanFrames awaiting operator approval for this session.
    ListPlans,
    /// Approve a plan (`None` = latest pending).
    ApprovePlan { plan_id: Option<String> },
    /// Return the active workbench to the orchestrator.
    ReturnToAgent { force: bool, message: Option<String> },
    /// Fork the current session into a new branch and switch to it.
    /// `at_turn` = `None` forks from the latest checkpoint; `message` is an
    /// optional branch message appended to the forked history.
    ForkSession {
        at_turn: Option<u64>,
        message: Option<String>,
    },
    /// List pending wiki proposals for this session.
    ListWikiProposals,
    /// Emergency-stop the current session and optionally redirect with a message.
    EmergencyStopAndRedirect { message: Option<String> },
    /// Run memory curation on the current session now, with optional operator
    /// focus notes that steer the curator's analysis.
    Curate { notes: Option<String> },
    /// Show resolved inference profile for the current session.
    ModelShow,
    /// Override the session inference preset until cleared.
    ModelSet { preset: String },
    /// Remove the session inference override.
    ModelClear,
    /// Anything else — the dispatcher surfaces a `✗` status.
    Unknown(String),
}

/// One-line hint while typing a slash command (full guide: `/help`).
pub const HELP_TEXT: &str =
    "/help all keys · /session · /fork · /plan · /return · /curate · /cron · /wiki · /test · /model · /quit · Esc cancel";

/// Full Session Room TUI reference — shown in the detail pane by `/help`.
pub fn help_lines() -> Vec<String> {
    vec![
        "Session Room — commands & keys".to_string(),
        String::new(),
        "Navigation".to_string(),
        "  j / ↓        scroll down".to_string(),
        "  k / ↑        scroll up".to_string(),
        "  PgDn / PgUp  page down / up (timeline or detail pane)".to_string(),
        "  g / Home     jump to oldest row".to_string(),
        "  G / End      jump to newest row (enable follow)".to_string(),
        "  [ / ]        jump to previous / next checkpoint row".to_string(),
        "  e / E        jump to next / previous attention row (errors, gates)".to_string(),
        "  Ctrl+F       search timeline · n / N cycle matches · Esc clear".to_string(),
        "  Y            copy selected row (tool token, else its text) to clipboard".to_string(),
        "  f / Space    toggle follow (pin to newest)".to_string(),
        "  Enter        event detail · plan review on plan row · answer pending question".to_string(),
        "  Esc          close detail / overlay · cancel quit · peek timeline from gate modal".to_string(),
        "               double-Esc (nothing open) = interrupt session".to_string(),
        "  h / l        horizontal scroll in detail pane".to_string(),
        "  ?            session info panel (stats, toggles, active gates)".to_string(),
        String::new(),
        "View".to_string(),
        "  a            cycle altitude floor (detail → normal → attention → error)".to_string(),
        "  s            toggle squash (fold routine detail events)".to_string(),
        "  R            toggle 💭 reasoning prefix on agent rows".to_string(),
        "  F            fork from selected row's turn & switch to the branch".to_string(),
        "               forkable turns show a cyan ═══ ⑂ fork ═══ divider".to_string(),
        String::new(),
        "Content & artifacts".to_string(),
        "  c            toggle live content tree (content.list)".to_string(),
        "  Enter/o      open selected content · artifact file list · view file".to_string(),
        "  m            comment on open content (prefix L12: or L12-14: for line hint)".to_string(),
        String::new(),
        "Gates (approval · wiki · escalation · plan · user.ask)".to_string(),
        "  y / n        approve/reject approval · wiki · escalation · plan (n = revision request)"
            .to_string(),
        "  p            open plan review for selected plan.pending row".to_string(),
        "  Enter/i/r    answer a pending user.ask (any row; newest ask wins)".to_string(),
        "  1–9          pick numbered option (interaction) · session from list · wiki proposal"
            .to_string(),
        "  g (modal)    leave timeline peek · return to gate resolve overlay".to_string(),
        String::new(),
        "Messaging".to_string(),
        "  i            compose operator message (multi-line editor)".to_string(),
        "               Enter send · Shift+Enter newline".to_string(),
        "               ←→↑↓ edit · Ctrl+V / Shift+Insert paste (multi-line) · Ctrl+C copy"
            .to_string(),
        String::new(),
        "Slash commands  (press / or : then type, Enter to run)".to_string(),
        "  /help  /?    this guide".to_string(),
        "  /quit  /q    exit (press q twice to confirm)".to_string(),
        "  /session <id>              switch to a root session".to_string(),
        "  /session list|ls [agent]   list recent sessions (1–9 to pick)".to_string(),
        "  /session resume|latest|last [agent]  jump to most recent session".to_string(),
        "  /fork [--at-turn N] [msg]  branch this session and switch to it".to_string(),
        "  /cron  /cron list|ls       scheduled jobs for this session".to_string(),
        "  /plan  /plan list          list pending PlanFrames".to_string(),
        "  /plan approve|a|ok [id]    approve a plan frame".to_string(),
        "  /return [--force] [note]   return the active workbench to the orchestrator".to_string(),
        "  /curate [focus notes]      run memory curation on this session now (notes steer the curator)".to_string(),
        "  /wiki  /wiki proposals|list|ls  list pending wiki proposals (1–9 detail)".to_string(),
        "  /test <scenario>           inject synthetic events (dev)".to_string(),
        "  /test help                 list test scenarios".to_string(),
        "  /estop [redirect message]  emergency-stop session · optionally re-send a message to redirect it".to_string(),
        "  /model                     show current inference profile".to_string(),
        "  /model <preset>            override model until cleared".to_string(),
        "  /model clear               remove the session override".to_string(),
        String::new(),
        "  q / Ctrl+C   quit (press twice within 3s · Esc cancels)".to_string(),
        String::new(),
        "Press Esc to close this pane.".to_string(),
    ]
}

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
        "fork" => parse_fork(tail),
        "cron" => parse_cron(tail),
        "plan" => parse_plan(tail),
        "return" => parse_return(tail),
        "curate" => parse_curate(tail),
        "wiki" => parse_wiki(tail),
        "test" => {
            let name = tail.trim().to_string();
            SlashCommand::Test { name }
        }
        "model" => parse_model(tail),
        "quit" | "q" | "exit" => SlashCommand::Quit,
        "help" | "?" => SlashCommand::Help,
        "estop" | "emergency-stop" => {
            let msg = tail.trim();
            SlashCommand::EmergencyStopAndRedirect {
                message: if msg.is_empty() { None } else { Some(msg.to_string()) },
            }
        }
        other => SlashCommand::Unknown(other.to_string()),
    }
}

fn parse_fork(tail: &str) -> SlashCommand {
    // `/fork`                         — fork current session from latest checkpoint
    // `/fork <message>`               — fork latest, append branch message
    // `/fork --at-turn N`             — fork from a specific turn's checkpoint
    // `/fork --at-turn N <message>`   — fork from turn N, append branch message
    let trimmed = tail.trim();
    let mut at_turn = None;
    let mut rest = trimmed;
    if let Some(after) = rest
        .strip_prefix("--at-turn")
        .or_else(|| rest.strip_prefix("--turn"))
    {
        let after = after.trim_start();
        let (num, remainder) = match after.split_once(char::is_whitespace) {
            Some((n, r)) => (n, r.trim()),
            None => (after, ""),
        };
        match num.parse::<u64>() {
            Ok(n) => {
                at_turn = Some(n);
                rest = remainder;
            }
            // `--at-turn` with no/invalid number is a usage error, not a branch
            // message — surface it rather than silently forking from latest.
            Err(_) => return SlashCommand::Unknown(format!("fork {trimmed}")),
        }
    }
    let message = (!rest.trim().is_empty()).then(|| rest.trim().to_string());
    SlashCommand::ForkSession { at_turn, message }
}

fn parse_cron(tail: &str) -> SlashCommand {
    let trimmed = tail.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("list") || trimmed.eq_ignore_ascii_case("ls")
    {
        SlashCommand::ListCronJobs
    } else {
        SlashCommand::Unknown(format!("cron {trimmed}"))
    }
}

fn parse_plan(tail: &str) -> SlashCommand {
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        return SlashCommand::ListPlans;
    }
    let (sub, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((h, r)) => (h.to_ascii_lowercase(), r.trim()),
        None => (trimmed.to_ascii_lowercase(), ""),
    };
    match sub.as_str() {
        "approve" | "a" | "ok" => SlashCommand::ApprovePlan {
            plan_id: (!rest.is_empty()).then(|| rest.to_string()),
        },
        _ => SlashCommand::Unknown(format!("plan {trimmed}")),
    }
}

fn parse_wiki(tail: &str) -> SlashCommand {
    let trimmed = tail.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("proposals")
        || trimmed.eq_ignore_ascii_case("list")
        || trimmed.eq_ignore_ascii_case("ls")
    {
        SlashCommand::ListWikiProposals
    } else {
        SlashCommand::Unknown(format!("wiki {trimmed}"))
    }
}

fn parse_return(tail: &str) -> SlashCommand {
    // `/return`                         — return active workbench
    // `/return --force`                 — force return, dropping unsaved edits
    // `/return -f`                      — short force flag
    // `/return some note here`          — return with operator note
    // `/return --force ship it`         — force return with note
    let trimmed = tail.trim();
    let mut rest = trimmed;
    let mut force = false;
    if let Some(after) = rest.strip_prefix("--force").or_else(|| rest.strip_prefix("-f")) {
        force = true;
        rest = after.trim();
    }
    let message = if rest.is_empty() { None } else { Some(rest.to_string()) };
    SlashCommand::ReturnToAgent { force, message }
}

fn parse_curate(tail: &str) -> SlashCommand {
    // `/curate`                         — curate the current session, no notes
    // `/curate focus on the retry loop` — curate with operator focus notes
    // The whole tail is free-text guidance passed to the memory-curator; it
    // steers the curator's scoring and graduation decisions (see its SKILL).
    let trimmed = tail.trim();
    let notes = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
    SlashCommand::Curate { notes }
}

fn parse_session(tail: &str) -> SlashCommand {
    // `/session <id>` and `/session list [agent]` and `/session resume [agent]`
    // — split on whitespace to peel off the sub-verb, then take the rest as a
    // free-form session id (most ids contain hyphens, so don't tokenize further).
    // The sub-verb is matched case-insensitively, but the captured id keeps its
    // original case — session ids are not normalized.
    let trimmed_tail = tail.trim_start();
    if trimmed_tail.is_empty() {
        return SlashCommand::SwitchSession(String::new());
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

fn parse_model(tail: &str) -> SlashCommand {
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        return SlashCommand::ModelShow;
    }
    if trimmed.eq_ignore_ascii_case("clear") || trimmed.eq_ignore_ascii_case("reset") {
        return SlashCommand::ModelClear;
    }
    SlashCommand::ModelSet {
        preset: trimmed.to_string(),
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
    fn help_lines_covers_slash_commands_and_navigation() {
        let text = help_lines().join("\n");
        for needle in [
            "/session list",
            "/session resume|latest|last",
            "/cron",
            "/wiki",
            "/plan approve",
            "/curate [focus notes]",
            "user.ask",
            "i            compose",
            "j / ↓",
            "PgDn / PgUp",
            "[ / ]",
            "?            session info",
            "c            toggle",
            "Enter/o      open",
            "g (modal)",
            "press / or :",
        ] {
            assert!(text.contains(needle), "help_lines missing {needle:?}");
        }
    }

    #[test]
    fn parse_session_switch_id() {
        // Bare `/session` (no arguments) → SwitchSession("") so the
        // dispatcher can surface a "missing id" error.
        assert_eq!(
            parse("/session"),
            SlashCommand::SwitchSession(String::new())
        );
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

    #[test]
    fn parse_cron_variants() {
        assert_eq!(parse("/cron"), SlashCommand::ListCronJobs);
        assert_eq!(parse("/cron list"), SlashCommand::ListCronJobs);
        assert_eq!(parse("/cron ls"), SlashCommand::ListCronJobs);
        assert_eq!(
            parse("/cron pause foo"),
            SlashCommand::Unknown("cron pause foo".into())
        );
    }

    #[test]
    fn parse_plan_variants() {
        assert_eq!(parse("/plan"), SlashCommand::ListPlans);
        assert_eq!(
            parse("/plan approve"),
            SlashCommand::ApprovePlan { plan_id: None }
        );
        assert_eq!(
            parse("/plan approve plan-abc123"),
            SlashCommand::ApprovePlan {
                plan_id: Some("plan-abc123".into())
            }
        );
        assert_eq!(
            parse("/plan a plan-xyz"),
            SlashCommand::ApprovePlan {
                plan_id: Some("plan-xyz".into())
            }
        );
        assert_eq!(
            parse("/plan cancel"),
            SlashCommand::Unknown("plan cancel".into())
        );
    }

    #[test]
    fn parse_fork_variants() {
        assert_eq!(
            parse("/fork"),
            SlashCommand::ForkSession {
                at_turn: None,
                message: None
            }
        );
        assert_eq!(
            parse("/fork try approach B"),
            SlashCommand::ForkSession {
                at_turn: None,
                message: Some("try approach B".into())
            }
        );
        assert_eq!(
            parse("/fork --at-turn 5"),
            SlashCommand::ForkSession {
                at_turn: Some(5),
                message: None
            }
        );
        assert_eq!(
            parse("/fork --at-turn 5 try approach B"),
            SlashCommand::ForkSession {
                at_turn: Some(5),
                message: Some("try approach B".into())
            }
        );
        assert_eq!(
            parse("/fork --turn 3"),
            SlashCommand::ForkSession {
                at_turn: Some(3),
                message: None
            }
        );
        // `--at-turn` with no/invalid number is a usage error.
        assert_eq!(parse("/fork --at-turn"), SlashCommand::Unknown("fork --at-turn".into()));
        assert_eq!(
            parse("/fork --at-turn abc"),
            SlashCommand::Unknown("fork --at-turn abc".into())
        );
    }

    #[test]
    fn parse_wiki_variants() {
        assert_eq!(parse("/wiki"), SlashCommand::ListWikiProposals);
        assert_eq!(parse("/wiki proposals"), SlashCommand::ListWikiProposals);
        assert_eq!(parse("/wiki list"), SlashCommand::ListWikiProposals);
        assert_eq!(parse("/wiki ls"), SlashCommand::ListWikiProposals);
        assert_eq!(
            parse("/wiki approve"),
            SlashCommand::Unknown("wiki approve".into())
        );
    }

    #[test]
    fn parse_test_variants() {
        assert_eq!(
            parse("/test user-ask"),
            SlashCommand::Test {
                name: "user-ask".into()
            }
        );
        assert_eq!(
            parse("/test full-session"),
            SlashCommand::Test {
                name: "full-session".into()
            }
        );
        assert_eq!(
            parse("/test help"),
            SlashCommand::Test {
                name: "help".into()
            }
        );
        assert_eq!(
            parse("/test"),
            SlashCommand::Test {
                name: String::new()
            }
        );
    }

    #[test]
    fn parse_model_show() {
        assert_eq!(parse("/model"), SlashCommand::ModelShow);
        assert_eq!(parse("/model  "), SlashCommand::ModelShow);
    }

    #[test]
    fn parse_model_set() {
        assert_eq!(
            parse("/model gpt-4o"),
            SlashCommand::ModelSet {
                preset: "gpt-4o".into()
            }
        );
        assert_eq!(
            parse("/model  default  "),
            SlashCommand::ModelSet {
                preset: "default".into()
            }
        );
    }

    #[test]
    fn parse_model_clear() {
        assert_eq!(parse("/model clear"), SlashCommand::ModelClear);
        assert_eq!(parse("/model reset"), SlashCommand::ModelClear);
        assert_eq!(parse("/model  clear  "), SlashCommand::ModelClear);
    }

    #[test]
    fn parse_return_variants() {
        assert_eq!(
            parse("/return"),
            SlashCommand::ReturnToAgent {
                force: false,
                message: None
            }
        );
        assert_eq!(
            parse("/return --force"),
            SlashCommand::ReturnToAgent {
                force: true,
                message: None
            }
        );
        assert_eq!(
            parse("/return -f"),
            SlashCommand::ReturnToAgent {
                force: true,
                message: None
            }
        );
        assert_eq!(
            parse("/return please review auth flow"),
            SlashCommand::ReturnToAgent {
                force: false,
                message: Some("please review auth flow".into())
            }
        );
        assert_eq!(
            parse("/return --force ship it"),
            SlashCommand::ReturnToAgent {
                force: true,
                message: Some("ship it".into())
            }
        );
    }

    #[test]
    fn parse_curate_variants() {
        // Bare `/curate` (and whitespace-only tail) → no notes.
        assert_eq!(parse("/curate"), SlashCommand::Curate { notes: None });
        assert_eq!(parse("/curate   "), SlashCommand::Curate { notes: None });
        // Free-text tail becomes the focus notes, verbatim (trimmed).
        assert_eq!(
            parse("/curate focus on the retry loop"),
            SlashCommand::Curate {
                notes: Some("focus on the retry loop".into())
            }
        );
        assert_eq!(
            parse("/curate looks like a missing approval, weight that"),
            SlashCommand::Curate {
                notes: Some("looks like a missing approval, weight that".into())
            }
        );
    }
}
