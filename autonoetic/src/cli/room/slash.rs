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
//! - `/agent <id> [reason...]` — hand the current session to another agent (#1088)
//! - `/session list [agent]` — list recent sessions (optionally filtered by agent)
//! - `/session resume` — switch to the most recent session
//! - `/cron` / `/cron list` — list scheduled jobs for the current session
//! - `/plan` / `/plan approve [id]` — list or approve pending PlanFrames
//! - `/return [--force] [note...]` — return the active workbench to the orchestrator
//! - `/curate [notes...]` — run memory curation on this session now (notes steer the curator)
//! - `/crystallize [notes...]` — make what worked here reusable (notes name the tactic)
//! - `/skills` — standing view of proposed skill work and what was decided
//! - `/audit` — per-turn egress audit for this session (what left, what was withheld)
//! - `/quit` / `/q` — exit the TUI
//! - `/help` / `/?` — full command reference in the detail pane
//! - `/model` — show current inference profile
//! - `/model <preset>` — set session-level model override
//! - `/model clear` — clear the override
//! - `/private` — toggle "this room is private" (provider constraint local_only)
//! - `/private <message>` — send one message marked `local_only` (never leaves the machine)
//! - `/taint <source>[:<path>] <label>` — declare a session-scoped egress rule
//!
//! See [`help_lines()`] for the complete key map and slash-command list.

/// Parsed slash-command. Variants carry their arguments so the dispatcher
/// doesn't re-parse strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// Switch to a specific root session id (already trimmed).
    SwitchSession(String),
    /// Hand the current session off to another agent (#1088).
    /// `reason` is the operator-facing motive recorded on the causal event.
    Handoff { target_agent_id: String, reason: Option<String> },
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
    /// Make what worked in the current session reusable, with optional operator
    /// notes naming the tactic. The crystallizer decides *which* durable home
    /// it gets (instruction, wrapper, or new skill) — see its SKILL.
    Crystallize { notes: Option<String> },
    /// Standing view of in-flight skill work: crystallization verdicts, lesson
    /// graduations, the decisions recorded against them, and the Candidate
    /// revisions waiting on the promotion gate.
    ListSkills,
    /// Per-turn egress audit for the watched session (RFC §9.3): what left the
    /// machine at each turn, what was withheld and why, and which
    /// declassifications were in force. Read from the causal chain via
    /// `egress.audit`; metadata only, never content.
    EgressAudit,
    /// Show resolved inference profile for the current session.
    ModelShow,
    /// Override the session inference preset until cleared.
    ModelSet { preset: String },
    /// Remove the session inference override.
    ModelClear,
    /// Send one message marked `local_only` (RFC §5.4 rung 3 — "this one message
    /// is private"). The gateway intersects the mark with the session policy
    /// default, so it can only restrict this turn's egress, never widen it.
    PrivateMessage { message: String },
    /// Toggle the room's provider constraint (RFC §5.4 rung 1). A bare
    /// `/private` pins the whole root session to `provider_constraint:
    /// local_only` — "this room is private"; run it again to lift the pin.
    /// Distinct from [`SlashCommand::PrivateMessage`], which marks one turn.
    RoomPrivate,
    /// Declare a session-scoped egress rule (RFC §5.4 rung 2):
    /// `/taint <source>[:<path>] <label>`. Session rules are intersected into
    /// the global set, so they can only restrict — a widening label is a usage
    /// error rather than a no-op.
    Taint {
        source: String,
        path: Option<String>,
        label: autonoetic_types::egress::NamedEgressLabel,
    },
    /// RFC §4.3 authoring aid (#978): `/local <intent>` — "emails stay local".
    /// The gateway proposes a concrete rule set from known tool catalogs +
    /// MCP servers; the operator confirms with one keystroke or edits. An
    /// unconfirmed proposal has no effect (Lawful-Executor §14).
    LocalIntent { intent: String },
    /// Anything else — the dispatcher surfaces a `✗` status.
    Unknown(String),
}

/// One-line hint while typing a slash command (full guide: `/help`).
pub const HELP_TEXT: &str =
    "/help all keys · /session · /fork · /plan · /return · /curate · /crystallize · /skills · /audit · /cron · /wiki · /test · /model · /private · /taint · /local · /quit · Esc cancel";

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
        "  u            jump to the `── N new ──` marker (rows added while you were scrolled away)".to_string(),
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
        "  a            cycle view floor (detail → normal → attention → error → story)".to_string(),
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
        "Grants".to_string(),
        "  G            toggle grants panel (active grants + egress taint)".to_string(),
        "               j/k · Home/End navigate · r revoke selected (press r twice to confirm)"
            .to_string(),
        String::new(),
        "Labels".to_string(),
        "  T            toggle labels panel (every labeled item in the session tree)".to_string(),
        "               j/k · Home/End navigate · f cycle filter · Enter open resolution detail"
            .to_string(),
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
        "  /session ⟨Tab⟩             Tab twice for a random friendly name".to_string(),
        "                             (the room is created by your first message)".to_string(),
        "  /agent <id> [reason...]    hand this session to another agent (root sessions)".to_string(),
        "  /session list|ls [agent]   list recent sessions (1–9 to pick)".to_string(),
        "  /session resume|latest|last [agent]  jump to most recent session".to_string(),
        "  /fork [--at-turn N] [msg]  branch this session and switch to it".to_string(),
        "  /cron  /cron list|ls       scheduled jobs for this session".to_string(),
        "  /plan  /plan list          list pending PlanFrames".to_string(),
        "  /plan approve|a|ok [id]    approve a plan frame".to_string(),
        "  /return [--force] [note]   return the active workbench to the orchestrator".to_string(),
        "  /curate [focus notes]      run memory curation on this session now (notes steer the curator)".to_string(),
        "  /crystallize [what worked]  make it reusable — instruction, wrapper, or new skill".to_string(),
        "  /skills                    proposed skill work: verdicts, decisions, candidates".to_string(),
        "  /audit                     egress audit: what left this machine per turn, and why".to_string(),
        "  /wiki  /wiki proposals|list|ls  list pending wiki proposals (1–9 detail)".to_string(),
        "  /test <scenario>           inject synthetic events (dev)".to_string(),
        "  /test help                 list test scenarios".to_string(),
        "  /estop [redirect message]  emergency-stop session · optionally re-send a message to redirect it".to_string(),
        "  /model                     show current inference profile".to_string(),
        "  /model <preset>            override model until cleared".to_string(),
        "  /model clear               remove the session override".to_string(),
        "  /private                   toggle \"this room is private\" — pin the whole".to_string(),
        "                             session to local_only (rung 1); run again to lift".to_string(),
        "  /private <message>         send one message marked local_only — the gateway".to_string(),
        "                             withholds it from any sink the label excludes".to_string(),
        "  /taint <source>[:<path>] <label>   declare a session egress rule (rung 2)".to_string(),
        "                             e.g. /taint email.* local_only or".to_string(),
        "                             /taint fs.read:~/mail/**.md no_remote_model".to_string(),
        "  /local <intent>            'emails stay local' → the gateway proposes".to_string(),
        "                             concrete rules from known tool catalogs and".to_string(),
        "                             MCP servers; press y to declare them (n/Esc".to_string(),
        "                             cancels — nothing applies unconfirmed)".to_string(),
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
        "agent" => parse_agent(tail),
        "fork" => parse_fork(tail),
        "cron" => parse_cron(tail),
        "plan" => parse_plan(tail),
        "return" => parse_return(tail),
        "curate" => parse_curate(tail),
        "crystallize" => parse_crystallize(tail),
        // No sub-verbs: the view is read-only, and acting on a row happens
        // through the promotion gate, not from here.
        "skills" => SlashCommand::ListSkills,
        // Read-only as well; an audit is a record, there is nothing to act on.
        "audit" => SlashCommand::EgressAudit,
        "wiki" => parse_wiki(tail),
        "test" => {
            let name = tail.trim().to_string();
            SlashCommand::Test { name }
        }
        "model" => parse_model(tail),
        // `/private` — a bare `/private` toggles the room's provider constraint
        // (rung 1: "this room is private"); `/private <text>` sends one
        // local_only message (rung 3). Empty-tail is the toggle, never an empty
        // labeled message, so a bare `/private` does something deliberate
        // instead of silently sending nothing.
        "private" => {
            let msg = tail.trim();
            if msg.is_empty() {
                SlashCommand::RoomPrivate
            } else {
                SlashCommand::PrivateMessage {
                    message: msg.to_string(),
                }
            }
        }
        // `/taint <source>[:<path>] <label>` — rung 2, a session-scoped rule.
        "taint" => parse_taint(tail),
        // `/local <intent>` — RFC §4.3 authoring aid (#978): "emails stay
        // local" → gateway proposes a concrete rule set → operator confirms
        // with one keystroke (y) or cancels (n/Esc). An empty intent stays a
        // `LocalIntent` so the TUI can render a specific "missing intent"
        // usage message instead of "unknown command".
        "local" => SlashCommand::LocalIntent {
            intent: tail.trim().to_string(),
        },
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

fn parse_crystallize(tail: &str) -> SlashCommand {
    // `/crystallize`                          — mine this session for a reusable tactic
    // `/crystallize the retry-with-backoff`   — name the tactic for the crystallizer
    // The tail is free text naming what the operator saw work; it is a strong
    // hint about *which* tactic to look at, not permission to skip the evidence
    // checks (see skill-crystallizer.default's SKILL).
    let trimmed = tail.trim();
    let notes = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
    SlashCommand::Crystallize { notes }
}

fn parse_agent(tail: &str) -> SlashCommand {
    // `/agent <id> [reason...]` (#1088) — hand the current session off to
    // another orchestrator. The first token is the target agent id; the rest,
    // if any, is the operator's reason (recorded on the causal event and
    // folded into the successor's context envelope).
    let trimmed = tail.trim();
    if trimmed.is_empty() {
        return SlashCommand::Handoff {
            target_agent_id: String::new(),
            reason: None,
        };
    }
    let (id, reason) = match trimmed.split_once(char::is_whitespace) {
        Some((id, rest)) => (id.trim(), rest.trim()),
        None => (trimmed, ""),
    };
    SlashCommand::Handoff {
        target_agent_id: id.to_string(),
        reason: (!reason.is_empty()).then(|| reason.to_string()),
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

/// Parse `/taint <source>[:<path>] <label>` into a session-scoped rule.
///
/// The label is the last whitespace token (labels never contain spaces); the
/// source/path split takes the *first* `:`, since tool names never contain one
/// and paths may. The label spelling reuses the CLI's `parse_named_label`
/// (accepts `local_only`/`local-only`, `no_remote_model`/`no-remote-model`,
/// `unrestricted`). `unrestricted` is rejected as a usage error — a session
/// rule can only restrict, so an unrestricted rule would be a no-op the
/// operator typed expecting it to do something.
fn parse_taint(tail: &str) -> SlashCommand {
    use autonoetic_types::egress::NamedEgressLabel;
    let trimmed = tail.trim();
    let (spec, label_raw) = match trimmed.rsplit_once(char::is_whitespace) {
        Some((s, l)) => (s, l.trim()),
        None => (trimmed, ""),
    };
    let label = match crate::cli::session::parse_named_label(label_raw) {
        Ok(l) => l,
        Err(_) => return SlashCommand::Unknown(format!("taint {trimmed}")),
    };
    if label == NamedEgressLabel::Unrestricted {
        return SlashCommand::Unknown(format!("taint {trimmed}"));
    }
    let (source, path) = match spec.split_once(':') {
        Some((s, p)) => (s.trim(), Some(p.trim().to_string())),
        None => (spec.trim(), None),
    };
    if source.is_empty() {
        return SlashCommand::Unknown(format!("taint {trimmed}"));
    }
    SlashCommand::Taint {
        source: source.to_string(),
        path: path.filter(|p| !p.is_empty()),
        label,
    }
}

/// Static catalog of top-level slash verbs with a one-line description —
/// drives the room TUI's type-ahead suggestion popup while the operator is
/// still typing the verb (before the first space). Order is display order
/// (roughly day-to-day usage first). Aliases (`q`, `exit`, `?`,
/// `emergency-stop`, `ls`) still parse but are intentionally not listed —
/// the popup teaches the canonical spelling.
pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "full command reference in the detail pane"),
    ("session", "switch session · Tab: random name · `list` / `resume`"),
    ("agent", "hand this session to another agent"),
    ("return", "return the active workbench to the orchestrator"),
    ("fork", "branch this session and switch to the fork"),
    ("plan", "list pending PlanFrames · `approve [id]`"),
    ("curate", "run memory curation on this session now"),
    ("crystallize", "make what worked here reusable"),
    ("skills", "proposed skill work: verdicts, decisions, candidates"),
    ("wiki", "list pending wiki proposals"),
    ("cron", "scheduled jobs for this session"),
    ("audit", "per-turn egress audit: what left, what was withheld"),
    ("model", "show / override the inference preset"),
    ("private", "toggle room privacy · or send one local_only message"),
    ("taint", "declare a session egress rule"),
    ("local", "describe intent → gateway proposes concrete rules"),
    ("estop", "emergency-stop session · optionally redirect"),
    ("test", "inject synthetic events (dev)"),
    ("quit", "exit the TUI"),
];

/// Prefix-matched command suggestions for the in-flight slash buffer (the
/// buffer without the leading `/`). Pure so the TUI and its renderer agree
/// without sharing state:
///
/// - empty buffer → every command (the full menu)
/// - no whitespace yet → case-insensitive prefix match on the verb
/// - any whitespace → empty: the operator is typing arguments, so the menu
///   closes and Enter/Tab fall through to the ordinary command path
///   (including `/taint` argument completion)
pub fn command_suggestions(buffer: &str) -> Vec<(&'static str, &'static str)> {
    let verb = buffer.trim_start();
    if verb.is_empty() {
        return COMMANDS.to_vec();
    }
    if verb.contains(char::is_whitespace) {
        return Vec::new();
    }
    let lower = verb.to_ascii_lowercase();
    COMMANDS
        .iter()
        .copied()
        .filter(|(name, _)| name.starts_with(&lower))
        .collect()
}

/// Random friendly session name for the `/session` Tab flow —
/// `adjective-noun-number` (e.g. `quiet-otter-4173`). Std-only randomness:
/// wall-clock nanos mixed with a process-wide counter through xorshift64*.
/// This names a room, it is not a security boundary — the id only has to be
/// memorable and not collide with an existing session in practice.
pub fn random_session_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    const GOLDEN: u64 = 0x9E3779B97F4A7C15;
    let mut s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(GOLDEN)
        ^ COUNTER
            .fetch_add(GOLDEN, Ordering::Relaxed)
            .wrapping_mul(0xBF58476D1CE4E5B9);
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    let r = s.wrapping_mul(0x2545F4914F6CDD1D);
    let adjective = NAME_ADJECTIVES[((r >> 32) as usize) % NAME_ADJECTIVES.len()];
    let noun = NAME_NOUNS[(r as usize) % NAME_NOUNS.len()];
    let number = ((r >> 16) % 10_000) as u16;
    format!("{adjective}-{noun}-{number:04}")
}

const NAME_ADJECTIVES: &[&str] = &[
    "amber", "bold", "brisk", "calm", "clever", "dawn", "eager", "fond", "gentle", "hazel",
    "keen", "lucid", "mellow", "noble", "quiet", "rapid", "sage", "steady", "tidal", "urban",
    "vivid", "warm", "witty", "zesty",
];

const NAME_NOUNS: &[&str] = &[
    "otter", "falcon", "maple", "harbor", "cinder", "willow", "sparrow", "basalt", "comet",
    "dune", "ember", "fern", "geyser", "heron", "island", "juniper", "kelp", "lantern",
    "meadow", "nimbus", "orchid", "pine", "quartz", "reef",
];

/// Live source catalog for `/taint` Tab-completion (#977) — the room fills it
/// from the `egress.sources` RPC (tool registry + MCP server list + path
/// families). Completion itself is a pure function of the buffer so it is
/// unit-testable without a gateway.
#[derive(Debug, Clone, Default)]
pub struct EgressSourceCatalog {
    pub tools: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub path_families: Vec<String>,
    /// Restrictive label spellings `/taint` accepts (`unrestricted` is a
    /// widening usage error and is never offered).
    pub labels: Vec<String>,
}

impl EgressSourceCatalog {
    /// Deduplicated, sorted completion candidates for the `source` slot:
    /// tools + path families + one `mcp.<server>.*` glob per registered server.
    pub fn source_candidates(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        out.extend(self.tools.iter().cloned());
        out.extend(self.path_families.iter().cloned());
        for s in &self.mcp_servers {
            out.push(format!("mcp.{s}.*"));
        }
        out.sort();
        out.dedup();
        out
    }
}

/// One Tab-completion result for `/taint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintCompletion {
    /// The completed slash buffer (without the leading `/`).
    pub buffer: String,
    /// How many candidates matched the prefix (1 ⇒ unambiguous).
    pub matches: usize,
    /// Match index for the next Tab press (cycles over `matches`).
    pub next_cycle: usize,
}

/// Tab-completion for the `/taint` command (RFC §5.4 rung 2, #977).
///
/// `buffer` is the in-flight slash buffer without its leading `/` (the room
/// keeps it that way; `parse` tolerates either spelling). `cycle` is the match
/// index selected by the previous Tab (0 = first). The last token is completed:
/// the source slot from [`EgressSourceCatalog::source_candidates`] (preserving
/// any `:path` suffix already typed), the label slot from the restrictive
/// spellings. Returns `None` when the buffer is not a `/taint` command, the
/// token is empty, or nothing matches the prefix.
pub fn taint_tab_complete(
    buffer: &str,
    catalog: &EgressSourceCatalog,
    cycle: usize,
) -> Option<TaintCompletion> {
    let body = buffer
        .trim_start()
        .strip_prefix('/')
        .unwrap_or(buffer.trim_start());
    let rest = body.strip_prefix("taint")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None; // some other `taint*` verb, e.g. /test
    }
    let tokens: Vec<&str> = rest.trim_start().split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    // A trailing space means the operator finished the previous token and is
    // positioning on the next slot: with a single token, Tab then completes
    // the (empty) label slot instead of re-completing the source — which
    // would swallow the trailing space and strand the operator in the source
    // slot.
    let trailing_space = rest.ends_with(char::is_whitespace);
    if tokens.len() == 1 && !trailing_space {
        // Source slot: `src` or `src:path` — complete the source part only,
        // preserving the path suffix the operator is still typing.
        let token = tokens[0];
        let (prefix, suffix) = match token.split_once(':') {
            Some((p, s)) => (p, s),
            None => (token, ""),
        };
        if prefix.is_empty() {
            return None;
        }
        let candidates = catalog.source_candidates();
        let matches: Vec<&str> = candidates
            .iter()
            .map(String::as_str)
            .filter(|s| s.starts_with(prefix))
            .collect();
        if matches.is_empty() {
            return None;
        }
        let idx = cycle % matches.len();
        let completed = if suffix.is_empty() {
            matches[idx].to_string()
        } else {
            format!("{}:{}", matches[idx], suffix)
        };
        Some(TaintCompletion {
            buffer: format!("taint {completed}"),
            matches: matches.len(),
            next_cycle: (cycle + 1) % matches.len(),
        })
    } else {
        // Label slot (second token; empty when the operator just typed the
        // trailing space — Tab then cycles the restrictive spellings).
        let token = tokens.get(1).copied().unwrap_or("");
        let matches: Vec<&str> = catalog
            .labels
            .iter()
            .map(String::as_str)
            .filter(|l| l.starts_with(token))
            .collect();
        if matches.is_empty() {
            return None;
        }
        let idx = cycle % matches.len();
        Some(TaintCompletion {
            buffer: format!("taint {} {}", tokens[0], matches[idx]),
            matches: matches.len(),
            next_cycle: (cycle + 1) % matches.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_handoff_variants() {
        // Bare `/agent` → empty target (dispatcher shows usage).
        assert_eq!(
            parse("/agent"),
            SlashCommand::Handoff { target_agent_id: String::new(), reason: None }
        );
        // Target only.
        assert_eq!(
            parse("/agent planner.collaborative"),
            SlashCommand::Handoff {
                target_agent_id: "planner.collaborative".to_string(),
                reason: None,
            }
        );
        // Target + free-form reason (kept verbatim, trimmed).
        assert_eq!(
            parse("/agent planner.collaborative  task needs plan co-editing "),
            SlashCommand::Handoff {
                target_agent_id: "planner.collaborative".to_string(),
                reason: Some("task needs plan co-editing".to_string()),
            }
        );
    }

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

    #[test]
    fn parse_crystallize_variants() {
        // Bare `/crystallize` (and whitespace-only tail) → no notes.
        assert_eq!(
            parse("/crystallize"),
            SlashCommand::Crystallize { notes: None }
        );
        assert_eq!(
            parse("/crystallize   "),
            SlashCommand::Crystallize { notes: None }
        );
        // Free text names the tactic the operator saw work, verbatim (trimmed).
        assert_eq!(
            parse("/crystallize the retry-with-backoff around the flaky API"),
            SlashCommand::Crystallize {
                notes: Some("the retry-with-backoff around the flaky API".into())
            }
        );
    }

    #[test]
    fn parse_skills_takes_no_arguments() {
        assert_eq!(parse("/skills"), SlashCommand::ListSkills);
        assert_eq!(parse("/skills   "), SlashCommand::ListSkills);
        // A tail is ignored rather than misread as a sub-verb — the view is
        // read-only, so there is nothing for an argument to mean yet.
        assert_eq!(parse("/skills pending"), SlashCommand::ListSkills);
        // Singular is a different word: report it rather than guess.
        assert!(matches!(parse("/skill"), SlashCommand::Unknown(_)));
    }

    #[test]
    fn parse_audit_takes_no_arguments() {
        assert_eq!(parse("/audit"), SlashCommand::EgressAudit);
        assert_eq!(parse("/audit   "), SlashCommand::EgressAudit);
        // Read-only view: a tail has nothing to mean, so it is ignored rather
        // than misread as a session id (which would audit the wrong session).
        assert_eq!(parse("/audit sess-other"), SlashCommand::EgressAudit);
        assert!(matches!(parse("/audits"), SlashCommand::Unknown(_)));
    }

    /// `/curate` and `/crystallize` share a prefix; the dispatcher must not
    /// route one to the other (a mis-route would fire the wrong agent on the
    /// operator's session).
    #[test]
    fn curate_and_crystallize_do_not_collide() {
        assert_eq!(parse("/curate"), SlashCommand::Curate { notes: None });
        assert_eq!(
            parse("/crystallize"),
            SlashCommand::Crystallize { notes: None }
        );
        // A partial word is neither — reported, never guessed.
        assert!(matches!(parse("/cryst"), SlashCommand::Unknown(_)));
    }

    /// `/private <message>` carries the message through verbatim — the whole
    /// tail, including internal whitespace and anything that looks like a flag,
    /// because it is prose the operator is sending, not arguments to parse.
    #[test]
    fn private_takes_the_whole_tail_as_the_message() {
        assert_eq!(
            parse("/private here is the email thread"),
            SlashCommand::PrivateMessage {
                message: "here is the email thread".to_string()
            }
        );
        // Surrounding whitespace trimmed, internal preserved.
        assert_eq!(
            parse("/private   two  spaces inside   "),
            SlashCommand::PrivateMessage {
                message: "two  spaces inside".to_string()
            }
        );
        // A leading `/` in the message body is not a nested command.
        assert_eq!(
            parse("/private see /etc/hosts"),
            SlashCommand::PrivateMessage {
                message: "see /etc/hosts".to_string()
            }
        );
    }

    /// A bare `/private` toggles the room's provider constraint (rung 1) — it is
    /// deliberately NOT an empty labeled message (rung 3). `/private <text>`
    /// keeps sending a one-turn local_only message.
    #[test]
    fn bare_private_is_room_toggle() {
        assert_eq!(parse("/private"), SlashCommand::RoomPrivate);
        assert_eq!(parse("/private   "), SlashCommand::RoomPrivate);
    }

    #[test]
    fn parse_taint_variants() {
        use autonoetic_types::egress::NamedEgressLabel;
        assert_eq!(
            parse("/taint email.* local_only"),
            SlashCommand::Taint {
                source: "email.*".into(),
                path: None,
                label: NamedEgressLabel::LocalOnly,
            }
        );
        assert_eq!(
            parse("/taint fs.read:~/mail/**.md no_remote_model"),
            SlashCommand::Taint {
                source: "fs.read".into(),
                path: Some("~/mail/**.md".into()),
                label: NamedEgressLabel::NoRemoteModel,
            }
        );
        // `local-only` (hyphenated) is accepted like the CLI.
        assert_eq!(
            parse("/taint  mcp.gmail.*  local-only  "),
            SlashCommand::Taint {
                source: "mcp.gmail.*".into(),
                path: None,
                label: NamedEgressLabel::LocalOnly,
            }
        );
    }

    /// `/taint` with no rule, an empty source, an unknown label, or the
    /// no-op `unrestricted` label is a usage error, never a silent no-op.
    #[test]
    fn malformed_taint_is_unknown() {
        assert!(matches!(parse("/taint"), SlashCommand::Unknown(_)));
        assert!(matches!(parse("/taint local_only"), SlashCommand::Unknown(_)));
        assert!(matches!(
            parse("/taint :path local_only"),
            SlashCommand::Unknown(_)
        ));
        assert!(matches!(
            parse("/taint email.* bogus_label"),
            SlashCommand::Unknown(_)
        ));
        assert!(matches!(
            parse("/taint email.* unrestricted"),
            SlashCommand::Unknown(_)
        ));
    }

    #[test]
    fn parse_local_intent_variants() {
        assert_eq!(
            parse("/local emails stay local"),
            SlashCommand::LocalIntent {
                intent: "emails stay local".into()
            }
        );
        assert_eq!(
            parse("/local   keep ~/mail off the network  "),
            SlashCommand::LocalIntent {
                intent: "keep ~/mail off the network".into()
            }
        );
        assert_eq!(
            parse("/local gmail"),
            SlashCommand::LocalIntent {
                intent: "gmail".into()
            }
        );
    }

    /// `/local` with no intent is a usage error — still a `LocalIntent` (not
    /// `Unknown`) so the TUI renders "missing intent" instead of "unknown
    /// command".
    #[test]
    fn empty_local_intent_parses_with_empty_intent() {
        assert!(matches!(
            parse("/local"),
            SlashCommand::LocalIntent { intent } if intent.is_empty()
        ));
        assert!(matches!(
            parse("/local  "),
            SlashCommand::LocalIntent { intent } if intent.is_empty()
        ));
    }
}

/// Minimal realistic catalog for completion tests: a couple of registry
/// tools, one MCP server, the path families, and the restrictive labels.
///
/// The `#[test]` functions below it are stripped from non-test builds, so
/// without this gate the helper compiles into the binary with no callers.
#[cfg(test)]
fn sample_catalog() -> EgressSourceCatalog {
    EgressSourceCatalog {
        tools: vec![
            "content.read".into(),
            "fs.read".into(),
            "sandbox.exec".into(),
            "web.fetch".into(),
        ],
        mcp_servers: vec!["gmail".into()],
        path_families: vec![
            "content.read".into(),
            "fs.read".into(),
            "sandbox.exec".into(),
            "artifact.exec".into(),
        ],
        labels: vec!["local_only".into(), "no_remote_model".into()],
    }
}

#[test]
fn taint_source_completes_tool_glob_and_family() {
    let cat = sample_catalog();
    // Tool prefix → the tool itself.
    let c = taint_tab_complete("taint sand", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint sandbox.exec");
    assert_eq!(c.matches, 1);
    assert_eq!(c.next_cycle, 0);
    // Path-family prefix → both the family and the tool match (sorted).
    let c = taint_tab_complete("taint fs.", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint fs.read");
    assert_eq!(c.matches, 1);
    // MCP prefix → the glob spelling.
    let c = taint_tab_complete("taint mcp.", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint mcp.gmail.*");
    assert_eq!(c.matches, 1);
}

#[test]
fn taint_source_path_suffix_is_preserved() {
    let cat = sample_catalog();
    let c = taint_tab_complete("taint sandbox.exec:~/mail/20", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint sandbox.exec:~/mail/20");
    assert_eq!(c.matches, 1);
}

#[test]
fn taint_source_cycling_rotates_over_matches() {
    let mut cat = sample_catalog();
    cat.tools = vec!["sandbox.exec".into(), "sandbox.clean".into()];
    let first = taint_tab_complete("taint sandbox.", &cat, 0).unwrap();
    assert_eq!(first.matches, 2);
    assert_eq!(first.next_cycle, 1);
    assert_eq!(first.buffer, "taint sandbox.clean");
    let second = taint_tab_complete("taint sandbox.", &cat, first.next_cycle).unwrap();
    assert_eq!(second.buffer, "taint sandbox.exec");
    assert_eq!(second.next_cycle, 0);
}

#[test]
fn taint_label_completes_restrictive_spellings_only() {
    let cat = sample_catalog();
    // `local_` → local_only; `unrestricted` is never offered.
    let c = taint_tab_complete("taint mcp.gmail.* local_", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint mcp.gmail.* local_only");
    assert_eq!(c.matches, 1);
    let c = taint_tab_complete("taint sandbox.exec no_", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint sandbox.exec no_remote_model");
    assert_eq!(c.matches, 1);
}

#[test]
fn taint_completion_refuses_non_taint_and_empty_tokens() {
    let cat = sample_catalog();
    // Other `taint*` verbs are not completed.
    assert_eq!(taint_tab_complete("test sand", &cat, 0), None);
    // No token yet.
    assert_eq!(taint_tab_complete("taint", &cat, 0), None);
    // `:` with an empty source part.
    assert_eq!(taint_tab_complete("taint :~/mail", &cat, 0), None);
    // No prefix match.
    assert_eq!(taint_tab_complete("taint zzz", &cat, 0), None);
    // Leading `/` is tolerated.
    let c = taint_tab_complete("/taint sand", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint sandbox.exec");
}

#[test]
fn source_candidates_are_sorted_and_deduped() {
    let cat = sample_catalog();
    let cands = cat.source_candidates();
    assert_eq!(
        cands,
        vec![
            "artifact.exec",
            "content.read",
            "fs.read",
            "mcp.gmail.*",
            "sandbox.exec",
            "web.fetch",
        ]
    );
}

#[test]
fn taint_tab_after_complete_source_enters_label_slot() {
    // `taint sandbox.exec ⟨Tab⟩`: the trailing space means the operator
    // finished the source — Tab completes the (empty) label slot instead of
    // re-completing the source and swallowing the trailing space.
    let cat = sample_catalog();
    let first = taint_tab_complete("taint sandbox.exec ", &cat, 0).unwrap();
    assert_eq!(first.buffer, "taint sandbox.exec local_only");
    assert_eq!(first.matches, 2, "both restrictive spellings offered");
    assert_eq!(first.next_cycle, 1);
    let second = taint_tab_complete("taint sandbox.exec ", &cat, first.next_cycle).unwrap();
    assert_eq!(second.buffer, "taint sandbox.exec no_remote_model");
    assert_eq!(second.next_cycle, 0);
    // A typed label prefix still filters the same cycle.
    let c = taint_tab_complete("taint sandbox.exec no_", &cat, 0).unwrap();
    assert_eq!(c.buffer, "taint sandbox.exec no_remote_model");
}

#[test]
fn taint_tab_completion_refuses_tainted_lookalike_verbs() {
    let cat = sample_catalog();
    // `tainted`/`taintx` must never complete — the TUI gates on the exact
    // verb, and the pure function must agree.
    assert_eq!(taint_tab_complete("tainted sand", &cat, 0), None);
    assert_eq!(taint_tab_complete("taintx mcp.gmail.* local_", &cat, 0), None);
}

#[test]
fn command_suggestions_empty_buffer_lists_the_full_menu() {
    let all = command_suggestions("");
    assert_eq!(all.len(), COMMANDS.len());
    // Every parse verb has a catalog entry, so the menu never suggests
    // something the parser would call unknown. `taint` is the one exception:
    // it requires arguments, so the bare verb is a usage error — it stays
    // listed because Tab completes it into the argument slot.
    for (name, _) in COMMANDS {
        if *name == "taint" {
            continue;
        }
        assert!(
            !matches!(parse(&format!("/{name}")), SlashCommand::Unknown(_)),
            "/{name} is listed in COMMANDS but parses as unknown"
        );
    }
}

#[test]
fn command_suggestions_prefix_filter_is_case_insensitive() {
    let names: Vec<&str> = command_suggestions("CR").into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["crystallize", "cron"]);
    let names: Vec<&str> = command_suggestions("Se").into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["session"]);
    // No match → empty menu (popup hidden).
    assert!(command_suggestions("zzz").is_empty());
}

#[test]
fn command_suggestions_close_once_arguments_begin() {
    // Whitespace means the verb is chosen and the operator is typing
    // arguments — the menu must close so Tab falls through to `/taint`
    // argument completion and Enter runs the buffer as typed.
    assert!(command_suggestions("taint ").is_empty());
    assert!(command_suggestions("taint fs.read local_").is_empty());
    assert!(command_suggestions("session list ").is_empty());
}

#[test]
fn random_session_name_is_well_formed() {
    let name = random_session_name();
    let parts: Vec<&str> = name.split('-').collect();
    assert_eq!(parts.len(), 3, "adjective-noun-number, got {name}");
    assert!(
        NAME_ADJECTIVES.contains(&parts[0]),
        "'{}' not from the adjective list",
        parts[0]
    );
    assert!(
        NAME_NOUNS.contains(&parts[1]),
        "'{}' not from the noun list",
        parts[1]
    );
    assert!(
        parts[2].len() == 4 && parts[2].bytes().all(|b| b.is_ascii_digit()),
        "number part must be zero-padded digits, got '{}'",
        parts[2]
    );
    // The generated name must round-trip the parser as a SwitchSession id.
    assert_eq!(
        parse(&format!("/session {name}")),
        SlashCommand::SwitchSession(name)
    );
}

#[test]
fn random_session_name_varies_across_calls() {
    let names: std::collections::HashSet<String> =
        (0..64).map(|_| random_session_name()).collect();
    assert!(
        names.len() > 1,
        "64 draws produced only {names:?} — the name is effectively constant"
    );
}
