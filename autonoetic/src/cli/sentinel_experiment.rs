//! Sentinel P4 — divergence-watchdog validation experiment harness.
//!
//! Implements the protocol described in
//! `docs/design/divergence-sentinel-validation.md`. Takes a labeled
//! corpus of archived sessions, gathers Layer 1's verdict per session
//! from the causal-events table, runs the watchdog blind per session,
//! and computes confusion matrices for both judges. Outputs a markdown
//! report.
//!
//! Caveats:
//! - The watchdog has live tools (`agent_message`, `session_escalate`)
//!   that write to the gateway store. Running this experiment against
//!   real sessions therefore creates real side-effect rows (notices to
//!   planners, operator escalations). For each session, the harness
//!   snapshots the undelivered-message count and pending-interaction
//!   count before and after the watchdog runs, and reports the deltas
//!   in a "Side-Effect Summary" section of the markdown report so the
//!   operator can clean up afterward.
//! - Layer 1's verdict for an archived session is "the highest
//!   divergence level reached during the session", read from the
//!   already-persisted `divergence.*` causal events.
//!
//! #1119 classification: **in-process executor embedding, by design.** The
//! harness builds `AgentExecutor`s (watchdog runs) that write real
//! side-effect rows through their own store handle, and snapshots
//! undelivered-message / pending-interaction counts around each run to
//! measure exactly those writes. RPC migration would break the measurement
//! the harness exists to make — exempted in `tests/cli_store_boundary.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use clap::Args;
use serde::{Deserialize, Serialize};

use autonoetic_gateway::llm::{build_driver, Message};
use autonoetic_gateway::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use autonoetic_gateway::runtime::session_overview::SessionOverview;
use autonoetic_gateway::runtime::tools::NativeToolRegistry;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_gateway::AgentRepository;

use crate::cli::watchdog::{run_watchdog, WatchdogRun};

/// CLI args for `autonoetic sentinel-experiment`.
#[derive(Args)]
pub struct SentinelExperimentArgs {
    /// Path to the labeled corpus YAML (see `docs/design/divergence-sentinel-validation.md`).
    #[arg(long)]
    pub corpus: PathBuf,
    /// Path to write the markdown report. Defaults to
    /// `docs/design/divergence-sentinel-validation.md.results.md` next
    /// to the corpus.
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// Skip the watchdog runs (use the cached fields in the corpus if
    /// present, else mark as `skipped`). Useful for re-running the
    /// analysis without re-spending LLM tokens.
    #[arg(long)]
    pub skip_watchdog: bool,
    /// Run the watchdog in tool-free mode: load `watchdog-fast.default`
    /// and pass an **empty** `NativeToolRegistry` to its executor, so
    /// no tools are exposed to the LLM at all (capabilities alone do
    /// not guarantee tool-free execution — several tools are
    /// always-available regardless of manifest capability set).
    /// Bundles a pre-rendered `SessionOverview` into the kickoff
    /// message and accepts a single LLM completion as the verdict.
    /// Roughly an order of magnitude cheaper than the default
    /// tool-using mode for complex sessions, and produces no
    /// side-effect rows on the target session because there are no
    /// callable tools.
    #[arg(long)]
    pub no_tools: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CorpusFile {
    pub sessions: Vec<CorpusEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CorpusEntry {
    pub session_id: String,
    /// Operator label — ground truth for the metrics. Accepts `"diverged"`
    /// or `"succeeded"` (other values are rejected at load time).
    pub label: String,
    #[serde(default)]
    pub notes: Option<String>,
    /// Optional pre-recorded watchdog reply, for re-running analysis
    /// without re-invoking the LLM. When present and `--skip-watchdog`
    /// is set, used directly.
    #[serde(default)]
    pub cached_watchdog_reply: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Diverged,
    Succeeded,
}

impl Label {
    fn parse(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "diverged" | "divergent" | "fail" | "failed" => Ok(Self::Diverged),
            "succeeded" | "success" | "healthy" | "ok" => Ok(Self::Succeeded),
            other => Err(anyhow::anyhow!(
                "unknown corpus label '{}' — accepted: diverged | succeeded",
                other
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Diverged => "diverged",
            Self::Succeeded => "succeeded",
        }
    }
}

/// One row of the comparison table.
#[derive(Debug, Clone, Serialize)]
pub struct ExperimentRow {
    pub session_id: String,
    pub label: String,
    pub notes: Option<String>,
    /// Highest divergence level (`watching` | `diverging` | `critical`)
    /// observed in the session's causal-event history. `None` when no
    /// `divergence.*` events were recorded.
    pub layer1_highest_level: Option<String>,
    /// Whether Layer 1 ever reached `diverging` or `critical` (treated
    /// as the "Layer 1 flagged" signal for the confusion matrix).
    pub layer1_flagged: bool,
    /// Full text reply from the watchdog (truncated to 4 KB in the report).
    pub watchdog_reply: Option<String>,
    /// Outcome tag from the watchdog run (`completed` | `error:...`).
    pub watchdog_outcome: String,
    /// Whether the watchdog produced a divergence judgment, based on
    /// keyword detection in its reply (`diverging`, `critical`,
    /// `divergence`, escalated, etc.). See [`classify_watchdog_reply`].
    pub watchdog_flagged: bool,
    /// Wall-clock seconds the watchdog took to produce its reply.
    pub watchdog_wall_secs: Option<f64>,
    /// Net new `agent_messages` from `sender_session_id="gateway:sentinel"`
    /// observed on this session after the watchdog ran. Operator-visible
    /// side effect that may need cleanup.
    pub new_sentinel_messages: u32,
    /// Net new pending `user_interactions` observed on this session
    /// after the watchdog ran. Operator-visible side effect that may
    /// need cleanup.
    pub new_pending_interactions: u32,
}

/// 2x2 confusion matrix counts.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Confusion {
    pub true_positive: u32,
    pub false_positive: u32,
    pub true_negative: u32,
    pub false_negative: u32,
}

impl Confusion {
    fn record(&mut self, label: Label, flagged: bool) {
        match (label, flagged) {
            (Label::Diverged, true) => self.true_positive += 1,
            (Label::Diverged, false) => self.false_negative += 1,
            (Label::Succeeded, true) => self.false_positive += 1,
            (Label::Succeeded, false) => self.true_negative += 1,
        }
    }

    fn tpr(&self) -> f64 {
        let positives = self.true_positive + self.false_negative;
        if positives == 0 {
            f64::NAN
        } else {
            self.true_positive as f64 / positives as f64
        }
    }

    fn fpr(&self) -> f64 {
        let negatives = self.false_positive + self.true_negative;
        if negatives == 0 {
            f64::NAN
        } else {
            self.false_positive as f64 / negatives as f64
        }
    }

    fn precision(&self) -> f64 {
        let predicted_positives = self.true_positive + self.false_positive;
        if predicted_positives == 0 {
            f64::NAN
        } else {
            self.true_positive as f64 / predicted_positives as f64
        }
    }
}

/// Decision rule per `docs/design/divergence-sentinel-design.md` §6
/// "Success criteria for proceeding to P3".
fn decision_rule(layer1: &Confusion, watchdog: &Confusion) -> Decision {
    let tpr_delta = watchdog.tpr() - layer1.tpr();
    let watchdog_fpr = watchdog.fpr();

    let tpr_pass = tpr_delta >= 0.20;
    let fpr_pass = watchdog_fpr <= 0.10;

    Decision {
        tpr_delta,
        watchdog_fpr,
        tpr_pass,
        fpr_pass,
        go: tpr_pass && fpr_pass,
    }
}

#[derive(Debug, Clone)]
struct Decision {
    tpr_delta: f64,
    watchdog_fpr: f64,
    tpr_pass: bool,
    fpr_pass: bool,
    go: bool,
}

/// Classify the watchdog's free-text reply as "flagged" vs "not flagged".
///
/// Two-tier detection:
/// 1. **Structured `VERDICT:` line** (produced by `watchdog-fast.default`):
///    `VERDICT: diverging` and `VERDICT: critical` → flagged;
///    `VERDICT: healthy` and `VERDICT: watching` → not flagged. This
///    takes priority when present since it's the agent's explicit
///    verdict.
/// 2. **Keyword fallback** (for the tool-using watchdog, which writes
///    free-form prose): look for `diverging`, `divergence`, `critical`,
///    `watching`, `loop pressure`, `failure pressure`, `repetition`,
///    `stalled`, `escalat`.
///
/// We deliberately use simple matching rather than a separate LLM call
/// so the harness stays cheap and deterministic. The decision surface
/// is documented and can be tightened later.
fn classify_watchdog_reply(reply: Option<&str>) -> bool {
    let Some(text) = reply else { return false };

    // Tier 1: structured VERDICT: line. Case-insensitive. Looks at the
    // first non-empty line of the reply for stability — the fast-mode
    // SKILL.md requires the verdict on line 1.
    for line in text.lines().take(5) {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        if let Some(rest) = lower.strip_prefix("verdict:") {
            let v = rest.trim();
            return matches!(v, "diverging" | "critical")
                || v.starts_with("diverging ") || v.starts_with("critical ");
        }
    }

    // Tier 2: free-form keyword fallback.
    let lower = text.to_lowercase();
    let positive_markers = [
        "diverging",
        "divergence",
        "critical",
        "watching",
        "loop pressure",
        "failure pressure",
        "repetition",
        "stalled",
        "escalat",
    ];
    positive_markers.iter().any(|m| lower.contains(m))
}

/// Run the tool-free fast watchdog against a target session: bundles a
/// pre-computed `SessionOverview` into the kickoff message and accepts a
/// single LLM completion as the verdict.
///
/// **Tool-free is enforced by passing an empty `NativeToolRegistry` to
/// the executor**, not by the manifest's empty `capabilities` list.
/// Several tools (`execution_search`, `session_escalate`,
/// `digest_annotate`, etc.) advertise `fn is_available(_) -> true`
/// regardless of capabilities, so `capabilities: []` alone would still
/// expose them. The empty registry forecloses that path: zero tools
/// declared to the LLM ⇒ zero tool calls ⇒ zero side-effect rows on
/// the target session.
async fn run_watchdog_fast(
    config_path: &Path,
    target_session_id: &str,
) -> anyhow::Result<WatchdogRun> {
    let loaded_config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_config = Arc::new(loaded_config);
    let gateway_dir = gateway_config.agents_dir.join(".gateway");
    let store = Arc::new(
        GatewayStore::open(&gateway_dir)
            .context("Failed to open GatewayStore — gateway must be initialised")?,
    );

    // Build the structured overview the watchdog will judge from.
    let overview = SessionOverview::for_session(&store, &gateway_dir, target_session_id);
    let overview_md = overview.render_markdown();

    let repo = AgentRepository::from_config(&gateway_config);
    let loaded = repo
        .get_sync("watchdog-fast.default")
        .context("Watchdog (fast) agent 'watchdog-fast.default' not found — run with a config that points to agents/specialists/watchdog-fast.default/")?;
    let manifest = loaded.manifest;
    let instructions = loaded.instructions;
    let agent_dir = loaded.dir;

    let llm_config = manifest
        .llm_config
        .clone()
        .context("watchdog-fast.default is missing llm_config in SKILL.md")?;
    let driver = build_driver(llm_config, reqwest::Client::new())?;

    // Empty registry — no tools at all. The manifest's `capabilities: []`
    // is necessary but not sufficient to disable tools because some are
    // always-available (e.g. `session_escalate`, `execution_search`); the
    // registry is the load-bearing isolation here.
    let mut runtime = AgentExecutor::new(
        manifest,
        instructions,
        driver,
        agent_dir,
        NativeToolRegistry::new(),
        Some(store),
    );
    runtime = runtime
        .with_gateway_dir(gateway_dir)
        .with_config(gateway_config)
        .with_initial_user_message(format!(
            "{}\n\n\
             ---\n\
             Produce your verdict on the first line as one of:\n\
             `VERDICT: healthy` | `VERDICT: watching` | `VERDICT: diverging` | `VERDICT: critical`\n\
             Then a blank line, then a one-paragraph justification (≤ 200 words) citing concrete \
             evidence from the overview above.",
            overview_md,
        ));

    let mut history = vec![
        Message::system(runtime.instructions.clone()),
        Message::user(runtime.initial_user_message.clone()),
    ];

    let result = match runtime.execute_with_history(&mut history).await {
        Ok(TurnOutcome::Completed(reply)) => WatchdogRun {
            reply,
            outcome_tag: "completed".to_string(),
        },
        Ok(other) => WatchdogRun {
            reply: None,
            outcome_tag: format!("interrupted:{:?}", other),
        },
        Err(e) => WatchdogRun {
            reply: None,
            outcome_tag: format!("error:{}", e),
        },
    };
    Ok(result)
}

pub async fn handle_sentinel_experiment(
    config_path: &std::path::Path,
    args: &SentinelExperimentArgs,
) -> anyhow::Result<()> {
    let corpus_text = std::fs::read_to_string(&args.corpus)
        .with_context(|| format!("failed to read corpus file {}", args.corpus.display()))?;
    let corpus: CorpusFile = serde_yaml::from_str(&corpus_text)
        .with_context(|| format!("failed to parse corpus YAML {}", args.corpus.display()))?;

    if corpus.sessions.is_empty() {
        anyhow::bail!("corpus file contains no sessions");
    }

    let loaded_config = autonoetic_gateway::config::load_config(config_path)?;
    let gateway_dir = loaded_config.agents_dir.join(".gateway");
    let store = Arc::new(
        GatewayStore::open(&gateway_dir)
            .context("Failed to open GatewayStore — the gateway must have been initialised at this path")?,
    );

    let mut rows: Vec<ExperimentRow> = Vec::with_capacity(corpus.sessions.len());
    let mut layer1 = Confusion::default();
    let mut watchdog_cm = Confusion::default();
    let mut parse_errors: Vec<String> = Vec::new();

    for entry in &corpus.sessions {
        let label = match Label::parse(&entry.label) {
            Ok(l) => l,
            Err(e) => {
                parse_errors.push(format!("session={}: {}", entry.session_id, e));
                continue;
            }
        };

        // Layer 1 verdict: highest divergence level reached.
        let (layer1_level, layer1_flagged) = highest_divergence_level(&store, &entry.session_id);

        // Snapshot side-effect-relevant state BEFORE the watchdog runs
        // so we can compute the delta and report a cleanup checklist.
        let (pre_msgs, pre_interactions) = snapshot_side_effects(&store, &entry.session_id);

        // Watchdog verdict: cached, tool-free, or full tool-using mode.
        let (watchdog_reply, watchdog_outcome, watchdog_wall_secs) = if args.skip_watchdog {
            match &entry.cached_watchdog_reply {
                Some(cached) => (Some(cached.clone()), "cached".to_string(), None),
                None => (None, "skipped".to_string(), None),
            }
        } else if args.no_tools {
            let started = Instant::now();
            let run = match run_watchdog_fast(config_path, &entry.session_id).await {
                Ok(r) => r,
                Err(e) => WatchdogRun {
                    reply: None,
                    outcome_tag: format!("error:{}", e),
                },
            };
            let elapsed = started.elapsed().as_secs_f64();
            (run.reply, run.outcome_tag, Some(elapsed))
        } else {
            let started = Instant::now();
            let run = match run_watchdog(config_path, &entry.session_id).await {
                Ok(r) => r,
                Err(e) => WatchdogRun {
                    reply: None,
                    outcome_tag: format!("error:{}", e),
                },
            };
            let elapsed = started.elapsed().as_secs_f64();
            (run.reply, run.outcome_tag, Some(elapsed))
        };

        let (post_msgs, post_interactions) = snapshot_side_effects(&store, &entry.session_id);
        let new_sentinel_messages = post_msgs.saturating_sub(pre_msgs);
        let new_pending_interactions = post_interactions.saturating_sub(pre_interactions);

        let watchdog_flagged = classify_watchdog_reply(watchdog_reply.as_deref());

        layer1.record(label, layer1_flagged);
        watchdog_cm.record(label, watchdog_flagged);

        rows.push(ExperimentRow {
            session_id: entry.session_id.clone(),
            label: label.as_str().to_string(),
            notes: entry.notes.clone(),
            layer1_highest_level: layer1_level.map(|l| l.to_string()),
            layer1_flagged,
            watchdog_reply,
            watchdog_outcome,
            watchdog_flagged,
            watchdog_wall_secs,
            new_sentinel_messages,
            new_pending_interactions,
        });
    }

    let decision = decision_rule(&layer1, &watchdog_cm);
    let report = render_report(&corpus, &rows, &layer1, &watchdog_cm, &decision, &parse_errors);

    let output = args.output.clone().unwrap_or_else(|| {
        let mut p = args.corpus.clone();
        if let Some(stem) = p.file_stem().map(|s| s.to_owned()) {
            let mut new_name = stem;
            new_name.push(".results.md");
            p.set_file_name(new_name);
        } else {
            p.set_extension("results.md");
        }
        p
    });
    std::fs::write(&output, &report)
        .with_context(|| format!("failed to write report to {}", output.display()))?;

    println!("Wrote validation report → {}", output.display());
    println!(
        "Decision: {} (TPR delta = {:+.3}, watchdog FPR = {:.3})",
        if decision.go { "GO (auto-invoke can ship)" } else { "NO-GO (stay manual)" },
        decision.tpr_delta,
        decision.watchdog_fpr,
    );
    Ok(())
}

/// Snapshot the counts of watchdog-attributable side-effect rows for
/// the given session. Returns `(undelivered_sentinel_messages,
/// pending_interactions)`. Errors are coerced to `(0, 0)` — a query
/// failure should not abort the experiment, only forfeit the delta
/// signal for that session.
fn snapshot_side_effects(store: &Arc<GatewayStore>, session_id: &str) -> (u32, u32) {
    let msgs = store
        .fetch_undelivered_messages(session_id)
        .map(|msgs| {
            msgs.iter()
                .filter(|m| m.sender_session_id.starts_with("gateway"))
                .count() as u32
        })
        .unwrap_or(0);
    let interactions = store
        .get_pending_interactions_for_session(session_id)
        .map(|v| v.len() as u32)
        .unwrap_or(0);
    (msgs, interactions)
}

/// Return the highest divergence level reached for the session, by
/// searching the `divergence.*` causal events. Returns `None` when no
/// such events exist. The "flagged" boolean is true iff the level is
/// `diverging` or `critical` (`watching` is observational only).
fn highest_divergence_level(
    store: &Arc<GatewayStore>,
    session_id: &str,
) -> (Option<&'static str>, bool) {
    let events = match store.search_causal_events(Some(session_id), None, 1000) {
        Ok(e) => e,
        Err(_) => return (None, false),
    };
    let mut rank: u8 = 0;
    for e in events.iter().filter(|e| e.category == "divergence") {
        let level = e
            .payload
            .as_ref()
            .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
            .and_then(|v| v.get("level").and_then(|l| l.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| e.action.clone());
        let lvl_rank: u8 = match level.as_str() {
            "watching" => 1,
            "diverging" => 2,
            "critical" => 3,
            _ => 0,
        };
        if lvl_rank > rank {
            rank = lvl_rank;
        }
    }
    match rank {
        1 => (Some("watching"), false),
        2 => (Some("diverging"), true),
        3 => (Some("critical"), true),
        _ => (None, false),
    }
}

fn render_report(
    corpus: &CorpusFile,
    rows: &[ExperimentRow],
    layer1: &Confusion,
    watchdog: &Confusion,
    decision: &Decision,
    parse_errors: &[String],
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let now = chrono::Utc::now().to_rfc3339();
    let _ = writeln!(out, "# Divergence Sentinel — Validation Report\n");
    let _ = writeln!(out, "Generated: `{}`  ", now);
    let _ = writeln!(out, "Corpus size: {} sessions  ", corpus.sessions.len());
    let _ = writeln!(out, "Skipped: {}  ", parse_errors.len());
    let _ = writeln!(out);

    let _ = writeln!(out, "## Decision\n");
    let verdict = if decision.go { "**GO** — auto-invoke meets the criteria." } else { "**NO-GO** — keep watchdog manual." };
    let _ = writeln!(out, "{}\n", verdict);
    let _ = writeln!(out, "| Criterion | Value | Pass? |");
    let _ = writeln!(out, "|---|---|---|");
    let _ = writeln!(out, "| TPR(watchdog) − TPR(layer 1) ≥ 0.20 | {:+.3} | {} |",
        decision.tpr_delta, if decision.tpr_pass { "✅" } else { "❌" });
    let _ = writeln!(out, "| FPR(watchdog) ≤ 0.10 | {:.3} | {} |",
        decision.watchdog_fpr, if decision.fpr_pass { "✅" } else { "❌" });
    let _ = writeln!(out);

    let _ = writeln!(out, "## Confusion Matrices\n");
    let _ = writeln!(out, "### Layer 1 (deterministic monitor)\n");
    render_confusion(&mut out, layer1);
    let _ = writeln!(out);
    let _ = writeln!(out, "### Watchdog (LLM)\n");
    render_confusion(&mut out, watchdog);
    let _ = writeln!(out);

    let _ = writeln!(out, "## Per-Session Results\n");
    let _ = writeln!(out, "| Session | Label | Layer 1 level | L1 flagged | Watchdog flagged | Watchdog outcome | Wall (s) | New msgs | New interactions |");
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|");
    for r in rows {
        let _ = writeln!(out, "| `{}` | {} | {} | {} | {} | `{}` | {} | {} | {} |",
            r.session_id,
            r.label,
            r.layer1_highest_level.as_deref().unwrap_or("—"),
            if r.layer1_flagged { "yes" } else { "no" },
            if r.watchdog_flagged { "yes" } else { "no" },
            r.watchdog_outcome,
            r.watchdog_wall_secs.map(|s| format!("{:.1}", s)).unwrap_or_else(|| "—".into()),
            r.new_sentinel_messages,
            r.new_pending_interactions,
        );
    }
    let _ = writeln!(out);

    // ── Side-effect summary ────────────────────────────────────────────
    let total_new_msgs: u32 = rows.iter().map(|r| r.new_sentinel_messages).sum();
    let total_new_interactions: u32 = rows.iter().map(|r| r.new_pending_interactions).sum();
    let sessions_with_msgs: Vec<&str> = rows
        .iter()
        .filter(|r| r.new_sentinel_messages > 0)
        .map(|r| r.session_id.as_str())
        .collect();
    let sessions_with_interactions: Vec<&str> = rows
        .iter()
        .filter(|r| r.new_pending_interactions > 0)
        .map(|r| r.session_id.as_str())
        .collect();
    let _ = writeln!(out, "## Side-Effect Summary\n");
    let _ = writeln!(out, "The watchdog has live messaging tools — when it judges a session divergent it writes real rows to the gateway store. The experiment ran against archived sessions, so this state is contamination that may need cleanup.\n");
    let _ = writeln!(out, "| Side-effect | Total new | Affected sessions |");
    let _ = writeln!(out, "|---|---|---|");
    let _ = writeln!(out, "| undelivered `agent_messages` from `gateway:sentinel` | {} | {} |",
        total_new_msgs,
        if sessions_with_msgs.is_empty() { "(none)".to_string() } else { sessions_with_msgs.join(", ") });
    let _ = writeln!(out, "| pending `user_interactions` | {} | {} |",
        total_new_interactions,
        if sessions_with_interactions.is_empty() { "(none)".to_string() } else { sessions_with_interactions.join(", ") });
    let _ = writeln!(out);
    if total_new_msgs > 0 || total_new_interactions > 0 {
        let _ = writeln!(out, "See `docs/design/divergence-sentinel-validation.md` §2.4 for the SQL cleanup snippet.\n");
    } else {
        let _ = writeln!(out, "No new side-effect rows recorded — the watchdog did not flag any session loudly. (Note: deltas use `fetch_undelivered_messages` + `get_pending_interactions_for_session`, so messages consumed by an active planner or interactions answered between snapshots will not be counted.)\n");
    }

    if !parse_errors.is_empty() {
        let _ = writeln!(out, "## Parse errors\n");
        for e in parse_errors {
            let _ = writeln!(out, "- {}", e);
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Watchdog Replies (truncated)\n");
    for r in rows {
        let _ = writeln!(out, "### `{}` ({})\n", r.session_id, r.label);
        if let Some(notes) = &r.notes {
            let _ = writeln!(out, "Notes: {}\n", notes);
        }
        match &r.watchdog_reply {
            Some(reply) => {
                let truncated: String = reply.chars().take(4000).collect();
                let _ = writeln!(out, "```\n{}\n```\n", truncated);
            }
            None => {
                let _ = writeln!(out, "_no reply ({})_\n", r.watchdog_outcome);
            }
        }
    }

    out
}

fn render_confusion(out: &mut String, c: &Confusion) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "|  | Predicted positive | Predicted negative |");
    let _ = writeln!(out, "|---|---|---|");
    let _ = writeln!(out, "| **Actual diverged** | {} (TP) | {} (FN) |", c.true_positive, c.false_negative);
    let _ = writeln!(out, "| **Actual succeeded** | {} (FP) | {} (TN) |", c.false_positive, c.true_negative);
    let _ = writeln!(out);
    let _ = writeln!(out, "TPR (recall): {:.3} · FPR: {:.3} · Precision: {:.3}",
        c.tpr(), c.fpr(), c.precision());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cm(tp: u32, fp: u32, tn: u32, fn_: u32) -> Confusion {
        Confusion { true_positive: tp, false_positive: fp, true_negative: tn, false_negative: fn_ }
    }

    #[test]
    fn confusion_records_quadrants() {
        let mut c = Confusion::default();
        c.record(Label::Diverged, true);  // TP
        c.record(Label::Diverged, false); // FN
        c.record(Label::Succeeded, true); // FP
        c.record(Label::Succeeded, false); // TN
        assert_eq!(c.true_positive, 1);
        assert_eq!(c.false_negative, 1);
        assert_eq!(c.false_positive, 1);
        assert_eq!(c.true_negative, 1);
    }

    #[test]
    fn tpr_fpr_precision_compute_correctly() {
        let c = cm(8, 1, 9, 2);
        // 8 TP + 2 FN = 10 positives; TPR = 8/10 = 0.8
        assert!((c.tpr() - 0.8).abs() < 1e-9);
        // 1 FP + 9 TN = 10 negatives; FPR = 1/10 = 0.1
        assert!((c.fpr() - 0.1).abs() < 1e-9);
        // 8 TP + 1 FP = 9 predicted positives; Precision = 8/9
        assert!((c.precision() - 8.0 / 9.0).abs() < 1e-9);
    }

    #[test]
    fn tpr_is_nan_when_no_positives() {
        let c = cm(0, 0, 5, 0);
        assert!(c.tpr().is_nan());
    }

    #[test]
    fn decision_rule_go_on_strong_watchdog() {
        // Layer 1: TPR 0.5 (5 TP, 5 FN). Watchdog: TPR 0.9 (9 TP, 1 FN).
        // Delta = 0.4 ≥ 0.20 ✓
        // Watchdog FPR: 0/10 = 0.0 ≤ 0.10 ✓
        let l1 = cm(5, 0, 10, 5);
        let wd = cm(9, 0, 10, 1);
        let d = decision_rule(&l1, &wd);
        assert!(d.go);
        assert!(d.tpr_pass);
        assert!(d.fpr_pass);
    }

    #[test]
    fn decision_rule_nogo_on_high_fpr() {
        // Strong recall but high FPR — should be NO-GO.
        let l1 = cm(5, 0, 10, 5);
        let wd = cm(9, 3, 7, 1); // FPR = 3/10 = 0.3 ✗
        let d = decision_rule(&l1, &wd);
        assert!(!d.go);
        assert!(d.tpr_pass);
        assert!(!d.fpr_pass);
    }

    #[test]
    fn decision_rule_nogo_on_marginal_tpr_delta() {
        // Watchdog only 10pp better than Layer 1 — under the 20pp threshold.
        let l1 = cm(7, 0, 10, 3);
        let wd = cm(8, 0, 10, 2);
        let d = decision_rule(&l1, &wd);
        assert!(!d.go);
        assert!(!d.tpr_pass);
        assert!(d.fpr_pass);
    }

    // ── Watchdog reply classification ──────────────────────────────────

    #[test]
    fn classify_silent_reply_is_not_flagged() {
        assert!(!classify_watchdog_reply(None));
        assert!(!classify_watchdog_reply(Some("")));
        assert!(!classify_watchdog_reply(Some("Session looks fine. Nothing to flag.")));
    }

    #[test]
    fn classify_divergence_keywords_flag() {
        assert!(classify_watchdog_reply(Some("This session is diverging — failure pressure is high.")));
        assert!(classify_watchdog_reply(Some("CRITICAL: planner repetition detected.")));
        assert!(classify_watchdog_reply(Some("I'm escalating this via session_escalate.")));
    }

    // ── Structured VERDICT: line (fast-watchdog output) ────────────────

    #[test]
    fn classify_structured_verdict_diverging_flags() {
        assert!(classify_watchdog_reply(Some("VERDICT: diverging\n\nLayer 1 flagged the session ...")));
        assert!(classify_watchdog_reply(Some("VERDICT: critical\n\n...")));
    }

    #[test]
    fn classify_structured_verdict_healthy_does_not_flag() {
        // Even if the justification body mentions "divergence" as a
        // negation ("no divergence observed"), the explicit VERDICT: line
        // takes priority.
        assert!(!classify_watchdog_reply(Some(
            "VERDICT: healthy\n\nNo divergence signals were observed; the session completed cleanly."
        )));
    }

    #[test]
    fn classify_structured_verdict_watching_does_not_flag() {
        // `watching` is the observational band — not "flagged" for the
        // confusion matrix (only `diverging` / `critical` flip the bit).
        assert!(!classify_watchdog_reply(Some(
            "VERDICT: watching\n\nOne signal in the warn band but no action band crossings."
        )));
    }

    #[test]
    fn classify_structured_verdict_is_case_insensitive() {
        assert!(classify_watchdog_reply(Some("verdict: DIVERGING\n\n...")));
        assert!(classify_watchdog_reply(Some("Verdict: Critical\n\n...")));
    }

    // ── Label parsing ──────────────────────────────────────────────────

    #[test]
    fn label_parse_accepts_synonyms() {
        assert_eq!(Label::parse("diverged").unwrap(), Label::Diverged);
        assert_eq!(Label::parse("Diverged").unwrap(), Label::Diverged);
        assert_eq!(Label::parse("FAILED").unwrap(), Label::Diverged);
        assert_eq!(Label::parse("succeeded").unwrap(), Label::Succeeded);
        assert_eq!(Label::parse("ok").unwrap(), Label::Succeeded);
    }

    #[test]
    fn label_parse_rejects_garbage() {
        assert!(Label::parse("maybe").is_err());
        assert!(Label::parse("").is_err());
    }
}
