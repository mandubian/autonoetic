//! Session-outcome auto-population + optional LLM grading
//! (Self-Improvement loop P0, #245).
//!
//! Two entry points, called in sequence at session close from
//! `execution.rs`:
//!
//! - [`write_session_outcome_metrics`] is **unconditional**. It pulls
//!   the auto-populated metrics (cost / tokens / turns / wall clock)
//!   from the runtime state and upserts a `session_outcomes` row. No
//!   LLM call, no config gate — every session gets a row.
//! - [`maybe_run_outcome_grader`] is **opt-in** via
//!   `GatewayConfig::outcome_grader.enabled`. Loads the configured
//!   grader agent (default `outcome-grader.default`), runs it against
//!   the session's digest, parses the `COMPLETION:` line from the
//!   reply, and attaches it to the row. The ownership invariant
//!   (`grader_agent_id != source_agent_id`) is checked at the gateway
//!   store layer.
//!
//! Both functions log on failure and never propagate — outcome
//! recording is observational, not transactional. A session that
//! closes cleanly must not fail because the outcome row didn't write.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::session_outcome::{Completion, SessionOutcome};

use crate::llm::{build_driver, Message};
use crate::runtime::lifecycle::{AgentExecutor, TurnOutcome};
use crate::runtime::session_overview::SessionOverview;
use crate::runtime::tools::NativeToolRegistry;
use crate::scheduler::gateway_store::GatewayStore;
use crate::AgentRepository;

/// Pull metrics from `runtime` and upsert the `session_outcomes` row.
/// Best-effort: errors are logged but never propagated.
pub fn write_session_outcome_metrics(
    runtime: &AgentExecutor,
    store: &Arc<GatewayStore>,
    session_id: &str,
    agent_id: &str,
) {
    let turns = runtime.turn_counter;

    // Tokens + cost: best-effort snapshot from the per-session budget
    // registry. Falls back to zero when no budget tracker is wired.
    let (tokens_total, cost_usd) = runtime
        .session_budget
        .as_ref()
        .and_then(|b| b.snapshot_counters(session_id))
        .map(|(_rounds, tokens, cost)| (tokens, cost))
        .unwrap_or((0, 0.0));

    // Wall clock: derive from `session_started_at` vs now. Both stored
    // as RFC3339 strings — parsing failures degrade to zero rather
    // than abort the session-end path.
    let wall_clock_secs = runtime
        .session_started_at
        .as_deref()
        .and_then(|started| chrono::DateTime::parse_from_rfc3339(started).ok())
        .map(|started| {
            let dur = chrono::Utc::now().signed_duration_since(started.with_timezone(&chrono::Utc));
            dur.num_milliseconds().max(0) as f64 / 1000.0
        })
        .unwrap_or(0.0);

    let root_session_id =
        crate::runtime::live_digest::base_session_id(session_id).to_string();

    if let Err(e) = store.upsert_session_outcome_metrics(
        session_id,
        &root_session_id,
        agent_id,
        None, // task_goal: not declared in P0 (future column)
        turns,
        tokens_total,
        cost_usd,
        wall_clock_secs,
    ) {
        tracing::warn!(
            target: "session_outcome",
            session_id = %session_id,
            error = %e,
            "failed to upsert session_outcomes metrics"
        );
    }
}

/// Run the outcome-grader agent (when enabled) against the session
/// and attach a `Completion` verdict to the existing row.
///
/// Gated by:
/// - `config.outcome_grader.enabled` (default `false`)
/// - `turn_count >= config.outcome_grader.min_turns`
/// - `session_suspended == false`
/// - Grader agent ID must differ from `source_agent_id` (ownership)
/// - `GatewayConfig::auto_learning.enabled` must be true (same gating
///   as the post-session digest — operator can mute the whole pipeline
///   in one place)
///
/// Errors at any stage are logged and swallowed; the row simply
/// retains `completion = unknown`.
pub async fn maybe_run_outcome_grader(
    config: &GatewayConfig,
    gateway_dir: &Path,
    store: Option<&Arc<GatewayStore>>,
    http_client: &reqwest::Client,
    session_id: &str,
    source_agent_id: &str,
    turn_count: u64,
    session_suspended: bool,
) {
    let Some(store) = store else { return };
    if !config.auto_learning.enabled {
        return;
    }
    let cfg = &config.outcome_grader;
    if !cfg.enabled {
        return;
    }
    if session_suspended {
        return;
    }
    if turn_count < cfg.min_turns as u64 {
        return;
    }
    if cfg.grader_agent_id == source_agent_id {
        tracing::warn!(
            target: "outcome_grader",
            session_id = %session_id,
            grader = %cfg.grader_agent_id,
            "ownership invariant: outcome_grader.grader_agent_id equals source_agent_id — skipping grading"
        );
        return;
    }

    if let Err(e) = run_outcome_grader_inner(
        config,
        gateway_dir,
        store,
        http_client,
        session_id,
        source_agent_id,
        &cfg.grader_agent_id,
    )
    .await
    {
        tracing::warn!(
            target: "outcome_grader",
            session_id = %session_id,
            error = %e,
            "outcome grading failed"
        );
    }
}

async fn run_outcome_grader_inner(
    config: &GatewayConfig,
    gateway_dir: &Path,
    store: &Arc<GatewayStore>,
    http_client: &reqwest::Client,
    session_id: &str,
    source_agent_id: &str,
    grader_agent_id: &str,
) -> anyhow::Result<()> {
    // The grader is *executed* — its instructions become the judgment prompt and
    // its `llm_config` picks the provider/model this call egresses to. Both must
    // come from the promoted revision, never from the ungated `agents_dir` copy
    // (#1136): an unvetted file must not be able to author a verdict that lands
    // in session outcomes, nor redirect the completion that produces it.
    let repo = AgentRepository::from_config(config);
    let loaded = repo
        .get_sync_from_store(grader_agent_id, gateway_dir, Some(store.as_ref()))
        .with_context(|| {
            // Deliberately does not name a cause: this lookup fails for a
            // missing alias, a missing revision directory, or an unparseable
            // SKILL.md alike. The wrapped error carries the specific reason.
            format!(
                "could not load grader agent '{}' from the revision store",
                grader_agent_id
            )
        })?;

    let llm_config = loaded
        .manifest
        .llm_config
        .clone()
        .with_context(|| format!("grader agent '{}' has no llm_config", grader_agent_id))?;
    let driver = build_driver(llm_config, http_client.clone())?;

    // Build the same SessionOverview the divergence watchdog uses —
    // it's the most concise structured view of what happened in the
    // session. The grader judges from this overview alone (tool-free).
    let overview = SessionOverview::for_session(store, gateway_dir, session_id);
    let overview_md = overview.render_markdown();

    let mut runtime = AgentExecutor::new(
        loaded.manifest,
        loaded.instructions,
        driver,
        loaded.dir,
        // Empty registry — the grader, like the fast watchdog, must
        // produce its verdict in a single completion with no tool
        // calls. This bounds cost and rules out side effects.
        NativeToolRegistry::new(),
        Some(store.clone()),
    );
    runtime = runtime
        .with_gateway_dir(gateway_dir.to_path_buf())
        .with_config(Arc::new(config.clone()))
        .with_initial_user_message(format!(
            "Grade the completion of session {}.\n\
             \n\
             {}\n\
             \n\
             ---\n\
             Produce your verdict on the first line as one of:\n\
             `COMPLETION: achieved` | `COMPLETION: partially_achieved` | `COMPLETION: failed` | `COMPLETION: aborted`\n\
             Then a blank line, then a one-paragraph evidence summary (≤ 150 words) citing concrete signals \
             from the overview above (tool histogram, errors, trajectory level).",
            session_id, overview_md,
        ));

    let mut history = vec![
        Message::system(runtime.instructions.clone()),
        Message::user(runtime.initial_user_message.clone()),
    ];

    let reply = match runtime.execute_with_history(&mut history).await? {
        TurnOutcome::Completed(reply) => reply,
        other => {
            tracing::warn!(
                target: "outcome_grader",
                session_id = %session_id,
                outcome = ?other,
                "grader did not complete cleanly"
            );
            return Ok(());
        }
    };
    let Some(text) = reply else {
        tracing::warn!(
            target: "outcome_grader",
            session_id = %session_id,
            "grader produced no reply"
        );
        return Ok(());
    };

    let (completion, evidence) = parse_grader_reply(&text);
    if completion == Completion::Unknown {
        tracing::warn!(
            target: "outcome_grader",
            session_id = %session_id,
            reply_preview = %text.chars().take(200).collect::<String>(),
            "grader did not emit a parseable COMPLETION: line — row remains 'unknown'"
        );
        return Ok(());
    }

    // Final defense-in-depth ownership check at the store boundary
    // (the gating above is config-level; this is row-level).
    let _ = SessionOutcome::check_grader_ownership(source_agent_id, grader_agent_id)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    store.set_session_outcome_grade(
        session_id,
        grader_agent_id,
        completion,
        evidence.as_deref(),
    )?;
    Ok(())
}

/// Parse the grader's free-text reply into `(Completion, evidence)`.
/// Looks for `COMPLETION: <slug>` on one of the first 5 lines
/// (case-insensitive). Everything after that line is taken as evidence,
/// trimmed to 500 chars. Returns `(Unknown, None)` when no marker
/// line is found.
fn parse_grader_reply(text: &str) -> (Completion, Option<String>) {
    let mut verdict_idx: Option<usize> = None;
    let mut verdict: Option<Completion> = None;
    for (i, line) in text.lines().take(5).enumerate() {
        let lower = line.trim().to_lowercase();
        if let Some(rest) = lower.strip_prefix("completion:") {
            verdict = Some(Completion::parse(rest));
            verdict_idx = Some(i);
            break;
        }
    }
    let Some(completion) = verdict else {
        return (Completion::Unknown, None);
    };
    // Evidence = text after the verdict line, trimmed.
    let evidence = verdict_idx.and_then(|i| {
        let after: String = text
            .lines()
            .skip(i + 1)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .chars()
            .take(500)
            .collect();
        if after.is_empty() {
            None
        } else {
            Some(after)
        }
    });
    (completion, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grader_reply_extracts_canonical_verdict() {
        let (c, e) = parse_grader_reply("COMPLETION: achieved\n\nPlanner reached the goal in 4 turns with no failures.");
        assert_eq!(c, Completion::Achieved);
        assert!(e.unwrap().contains("Planner reached"));
    }

    #[test]
    fn parse_grader_reply_is_case_insensitive() {
        let (c, _) = parse_grader_reply("completion: Failed\n\nBlah.");
        assert_eq!(c, Completion::Failed);
    }

    #[test]
    fn parse_grader_reply_accepts_aliases() {
        let (c, _) = parse_grader_reply("COMPLETION: success");
        assert_eq!(c, Completion::Achieved);
        let (c, _) = parse_grader_reply("COMPLETION: cancelled");
        assert_eq!(c, Completion::Aborted);
    }

    #[test]
    fn parse_grader_reply_returns_unknown_on_missing_marker() {
        let (c, e) = parse_grader_reply("This session went well overall, the agent achieved most goals.");
        assert_eq!(c, Completion::Unknown);
        assert!(e.is_none());
    }

    #[test]
    fn parse_grader_reply_handles_marker_on_later_line() {
        // Verdict on line 3 (within first 5) is accepted.
        let (c, _) = parse_grader_reply("Reviewing session.\nAnalyzing tools.\nCOMPLETION: partially_achieved\n\nMost goals met.");
        assert_eq!(c, Completion::PartiallyAchieved);
    }

    #[test]
    fn parse_grader_reply_ignores_marker_past_first_5_lines() {
        // Anti-hallucination: if the verdict is buried deep, do not
        // surface it — operator should see "unknown" and investigate.
        let text = "line1\nline2\nline3\nline4\nline5\nCOMPLETION: achieved\nevidence";
        let (c, _) = parse_grader_reply(text);
        assert_eq!(c, Completion::Unknown);
    }

    #[test]
    fn parse_grader_reply_caps_evidence_at_500_chars() {
        let huge_evidence = "x".repeat(2000);
        let text = format!("COMPLETION: achieved\n\n{}", huge_evidence);
        let (_, e) = parse_grader_reply(&text);
        assert_eq!(e.unwrap().len(), 500);
    }
}
