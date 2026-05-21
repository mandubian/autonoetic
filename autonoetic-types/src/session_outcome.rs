//! `SessionOutcome` — structured per-session record (Self-Improvement P0).
//!
//! One row per session, written when the session terminates. Carries
//! both the **auto-populated metrics** that every session has (cost,
//! tokens, turns, wall clock) and the **graded judgment** an independent
//! outcome-grader agent attaches afterwards (`completion`, evidence).
//! An operator can attach an explicit thumbs-up/thumbs-down rating via
//! `autonoetic session rate <id>`.
//!
//! Downstream phases of the self-improvement loop consume this row as
//! the single ground-truth signal for "did this session succeed?":
//!
//! - P1 (`eval_compare`) reads aggregated counts of `completion` +
//!   `cost_usd` deltas to decide GO/NO-GO on a candidate revision.
//! - P2 (A/B replay) tags each replayed session with a `SessionOutcome`
//!   so the comparator sees apples-to-apples grades.
//! - P3 (`autonoetic improve` CLI) surfaces the outcome to the operator
//!   for interactive review.
//!
//! **Ownership invariant** (mirrors the one already in
//! `eval_suite_publish.evaluated_targets`): the grader agent must NOT
//! be the agent that ran the session. Self-grading would defeat the
//! "honest judge" property that makes the outcome trustworthy.

use serde::{Deserialize, Serialize};

/// Graded completion of a session — the LLM grader's verdict on
/// whether the session achieved its task goal.
///
/// `Unknown` is the default when no grader has run and no operator
/// rating overrides it. Treating "ungraded" as a distinct state (rather
/// than as `Failed`) avoids silently flipping the metric when the
/// grader is disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Achieved,
    PartiallyAchieved,
    Failed,
    Aborted,
    Unknown,
}

impl Completion {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Achieved => "achieved",
            Self::PartiallyAchieved => "partially_achieved",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a string verdict (typically the value after `COMPLETION:`
    /// in the grader's reply). Accepts the canonical slug + a handful
    /// of common aliases. Returns `Unknown` for anything unrecognised.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "achieved" | "success" | "succeeded" => Self::Achieved,
            "partially_achieved" | "partial" | "partially" => Self::PartiallyAchieved,
            "failed" | "fail" | "failure" => Self::Failed,
            "aborted" | "abort" | "cancelled" => Self::Aborted,
            _ => Self::Unknown,
        }
    }
}

/// Explicit operator thumb. Kept tiny on purpose — a deeper rating
/// scale would need calibration before downstream phases can trust
/// the signal. The `note` field lets the operator add free-text
/// context without us inventing a structured taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorThumb {
    Up,
    Down,
}

impl OperatorThumb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "up" | "thumbs_up" | "thumbs-up" => Some(Self::Up),
            "down" | "thumbs_down" | "thumbs-down" => Some(Self::Down),
            _ => None,
        }
    }
}

/// Operator rating attached after the fact via `autonoetic session
/// rate`. Separate from `Completion` so that "operator disagreed with
/// the grader" is recoverable evidence rather than a silent overwrite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorRating {
    pub thumb: OperatorThumb,
    /// Optional free-text. Bounded at the CLI layer; not by the type.
    #[serde(default)]
    pub note: Option<String>,
    pub rated_at: String,
}

/// Who graded this outcome + when. Empty when the grader is disabled
/// or hasn't run yet (the row still carries the auto-populated metrics).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraderProvenance {
    /// Agent ID of the grader. Must NOT equal the run agent's ID
    /// (ownership invariant — see module docs).
    pub grader_agent_id: String,
    pub graded_at: String,
    /// Short evidence string the grader produced (≤ 500 chars in
    /// practice; the type does not cap, the persistence layer does).
    #[serde(default)]
    pub evidence_summary: Option<String>,
}

/// Cumulative LLM token usage across the whole session. P0 stores the
/// flat sum; P1+ may break out input/output/cached if they prove
/// useful for cost-vs-quality analysis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBreakdown {
    pub total: u64,
}

/// Domain shape, decoupled from the SQLite row layout. The
/// gateway store maps to/from `SessionOutcomeRecord` (defined in
/// `autonoetic-gateway`); this struct is what tools and CLI see.
///
/// Note: `PartialEq` only (not `Eq`) because `cost_usd` and
/// `wall_clock_secs` are `f64`. Tests use `assert_eq!` which only
/// needs `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOutcome {
    pub outcome_id: String,
    pub session_id: String,
    pub root_session_id: String,
    /// Source agent — the agent that *ran* the session.
    pub source_agent_id: String,
    /// Operator-stated goal, if one was declared. Optional because not
    /// every session has an explicit goal field today.
    #[serde(default)]
    pub task_goal: Option<String>,

    // ── Auto-populated metrics ────────────────────────────────────────
    pub completion: Completion,
    pub turns: u64,
    pub tokens: TokenBreakdown,
    pub cost_usd: f64,
    pub wall_clock_secs: f64,

    // ── Optional graded / rated overlays ──────────────────────────────
    #[serde(default)]
    pub grader: Option<GraderProvenance>,
    #[serde(default)]
    pub operator_rating: Option<OperatorRating>,

    pub created_at: String,
    /// Updated whenever the row is touched (grade write, operator rating
    /// write). Lets downstream tools detect stale snapshots.
    pub updated_at: String,
}

impl SessionOutcome {
    /// Convenience: did this session succeed? Per the multi-axis rule
    /// the self-improvement loop uses, **operator rating wins** when
    /// present, otherwise the grader's completion is consulted. The
    /// auto-populated metrics never directly imply success — a
    /// "completed in 12k tokens" session might still be a failure.
    pub fn judged_success(&self) -> Option<bool> {
        if let Some(rating) = &self.operator_rating {
            return Some(matches!(rating.thumb, OperatorThumb::Up));
        }
        match self.completion {
            Completion::Achieved => Some(true),
            Completion::PartiallyAchieved => Some(true),
            Completion::Failed | Completion::Aborted => Some(false),
            Completion::Unknown => None,
        }
    }

    /// Mirror of the eval-suite ownership invariant. Returns `Err` when
    /// `candidate_grader_id` equals `source_agent_id`, intended to be
    /// called by the gateway right before writing a grade.
    pub fn check_grader_ownership(
        source_agent_id: &str,
        candidate_grader_id: &str,
    ) -> Result<(), String> {
        if candidate_grader_id == source_agent_id {
            Err(format!(
                "Ownership violation: agent '{}' cannot grade its own session. \
                 The outcome grader must be a different agent than the one that ran the session.",
                candidate_grader_id
            ))
        } else {
            Ok(())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Completion parsing ─────────────────────────────────────────────

    #[test]
    fn completion_parses_canonical_slugs() {
        assert_eq!(Completion::parse("achieved"), Completion::Achieved);
        assert_eq!(Completion::parse("partially_achieved"), Completion::PartiallyAchieved);
        assert_eq!(Completion::parse("failed"), Completion::Failed);
        assert_eq!(Completion::parse("aborted"), Completion::Aborted);
        assert_eq!(Completion::parse("unknown"), Completion::Unknown);
    }

    #[test]
    fn completion_parses_common_aliases() {
        assert_eq!(Completion::parse("success"), Completion::Achieved);
        assert_eq!(Completion::parse("partial"), Completion::PartiallyAchieved);
        assert_eq!(Completion::parse("fail"), Completion::Failed);
        assert_eq!(Completion::parse("cancelled"), Completion::Aborted);
    }

    #[test]
    fn completion_parse_is_case_insensitive_and_trims() {
        assert_eq!(Completion::parse("  Achieved  "), Completion::Achieved);
        assert_eq!(Completion::parse("FAILED"), Completion::Failed);
    }

    #[test]
    fn completion_unknown_on_garbage() {
        assert_eq!(Completion::parse(""), Completion::Unknown);
        assert_eq!(Completion::parse("maybe"), Completion::Unknown);
    }

    // ── OperatorThumb parsing ─────────────────────────────────────────

    #[test]
    fn thumb_parses_canonical_and_hyphenated() {
        assert_eq!(OperatorThumb::parse("up"), Some(OperatorThumb::Up));
        assert_eq!(OperatorThumb::parse("thumbs-up"), Some(OperatorThumb::Up));
        assert_eq!(OperatorThumb::parse("THUMBS_DOWN"), Some(OperatorThumb::Down));
        assert_eq!(OperatorThumb::parse("garbage"), None);
    }

    // ── Ownership invariant ───────────────────────────────────────────

    #[test]
    fn grader_ownership_rejects_self_grading() {
        let r = SessionOutcome::check_grader_ownership("planner.default", "planner.default");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("Ownership violation"));
    }

    #[test]
    fn grader_ownership_allows_independent_grader() {
        let r = SessionOutcome::check_grader_ownership(
            "planner.default",
            "outcome-grader.default",
        );
        assert!(r.is_ok());
    }

    // ── judged_success priority ───────────────────────────────────────

    fn fixture(completion: Completion, rating: Option<OperatorThumb>) -> SessionOutcome {
        SessionOutcome {
            outcome_id: "o".into(),
            session_id: "s".into(),
            root_session_id: "s".into(),
            source_agent_id: "planner.default".into(),
            task_goal: None,
            completion,
            turns: 0,
            tokens: TokenBreakdown::default(),
            cost_usd: 0.0,
            wall_clock_secs: 0.0,
            grader: None,
            operator_rating: rating.map(|t| OperatorRating {
                thumb: t,
                note: None,
                rated_at: "2026-05-21T00:00:00Z".into(),
            }),
            created_at: "2026-05-21T00:00:00Z".into(),
            updated_at: "2026-05-21T00:00:00Z".into(),
        }
    }

    #[test]
    fn judged_success_operator_thumbs_up_overrides_failed_grade() {
        let outcome = fixture(Completion::Failed, Some(OperatorThumb::Up));
        assert_eq!(outcome.judged_success(), Some(true));
    }

    #[test]
    fn judged_success_operator_thumbs_down_overrides_achieved_grade() {
        let outcome = fixture(Completion::Achieved, Some(OperatorThumb::Down));
        assert_eq!(outcome.judged_success(), Some(false));
    }

    #[test]
    fn judged_success_partial_counts_as_success_in_binary_view() {
        // P0 keeps the binary view simple; finer distinctions (e.g., a
        // half-credit weighting) can land in a future axis if needed.
        let outcome = fixture(Completion::PartiallyAchieved, None);
        assert_eq!(outcome.judged_success(), Some(true));
    }

    #[test]
    fn judged_success_unknown_returns_none_not_false() {
        let outcome = fixture(Completion::Unknown, None);
        assert_eq!(outcome.judged_success(), None);
    }

    // ── Serde round-trip (so the JSON payload shape is stable) ───────

    #[test]
    fn session_outcome_serde_round_trips() {
        let outcome = fixture(Completion::Achieved, Some(OperatorThumb::Up));
        let json = serde_json::to_string(&outcome).unwrap();
        let back: SessionOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }
}
