//! `session_outcomes` table — one row per session, written at session
//! close. Carries auto-populated metrics + an optional graded overlay
//! (Self-Improvement loop P0, #245).
//!
//! Ownership invariant (mirrors `eval_suite_publish.evaluated_targets`):
//! the grader's `agent_id` must NOT equal the session's
//! `source_agent_id`. Enforced at write time by
//! [`GatewayStore::set_session_outcome_grade`].

use anyhow::{Context, Result};
use autonoetic_types::session_outcome::{
    Completion, GraderProvenance, OperatorRating, OperatorThumb, SessionOutcome, TokenBreakdown,
};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::GatewayStore;

/// Storage row shape for `session_outcomes`. Mirrors the SQL columns
/// 1-for-1; conversion to the domain `SessionOutcome` happens via
/// [`SessionOutcomeRecord::into_domain`].
#[derive(Debug, Clone, Serialize)]
pub struct SessionOutcomeRecord {
    pub outcome_id: String,
    pub session_id: String,
    pub root_session_id: String,
    pub source_agent_id: String,
    pub task_goal: Option<String>,
    pub completion: String,
    pub turns: i64,
    pub tokens_total: i64,
    pub cost_usd: f64,
    pub wall_clock_secs: f64,
    pub grader_agent_id: Option<String>,
    pub graded_at: Option<String>,
    pub grader_evidence: Option<String>,
    pub operator_thumb: Option<String>,
    pub operator_note: Option<String>,
    pub operator_rated_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl SessionOutcomeRecord {
    pub fn into_domain(self) -> SessionOutcome {
        SessionOutcome {
            outcome_id: self.outcome_id,
            session_id: self.session_id,
            root_session_id: self.root_session_id,
            source_agent_id: self.source_agent_id,
            task_goal: self.task_goal,
            completion: Completion::parse(&self.completion),
            // Clamp negatives to 0. The schema column is INTEGER (signed
            // i64) but the value is conceptually a count; treating a
            // corrupted negative as a wrapped huge u64 would produce
            // nonsensical metrics downstream.
            turns: self.turns.max(0) as u64,
            tokens: TokenBreakdown {
                total: self.tokens_total.max(0) as u64,
            },
            cost_usd: self.cost_usd,
            wall_clock_secs: self.wall_clock_secs,
            grader: match (self.grader_agent_id, self.graded_at) {
                (Some(id), Some(ts)) => Some(GraderProvenance {
                    grader_agent_id: id,
                    graded_at: ts,
                    evidence_summary: self.grader_evidence,
                }),
                _ => None,
            },
            operator_rating: match (self.operator_thumb.as_deref(), self.operator_rated_at) {
                (Some(t), Some(ts)) => OperatorThumb::parse(t).map(|thumb| OperatorRating {
                    thumb,
                    note: self.operator_note,
                    rated_at: ts,
                }),
                _ => None,
            },
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

impl GatewayStore {
    /// Insert (or replace) the auto-populated `SessionOutcome` row for
    /// a session. Idempotent on `session_id` — calling twice for the
    /// same session updates the existing row (preserving any prior
    /// grader / operator-rating overlays via the `COALESCE` clauses).
    ///
    /// `completion` defaults to `unknown` here — the grader's verdict
    /// is set separately by [`GatewayStore::set_session_outcome_grade`].
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session_outcome_metrics(
        &self,
        session_id: &str,
        root_session_id: &str,
        source_agent_id: &str,
        task_goal: Option<&str>,
        turns: u64,
        tokens_total: u64,
        cost_usd: f64,
        wall_clock_secs: f64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let outcome_id = format!("so_{:x}", uuid::Uuid::new_v4().as_u128());
        // ON CONFLICT(session_id) preserves grader / operator overlays
        // that may already be present, and refreshes only the
        // auto-populated metrics. The `created_at` is preserved.
        conn.execute(
            "INSERT INTO session_outcomes \
             (outcome_id, session_id, root_session_id, source_agent_id, task_goal, \
              completion, turns, tokens_total, cost_usd, wall_clock_secs, \
              created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'unknown', ?6, ?7, ?8, ?9, ?10, ?10) \
             ON CONFLICT(session_id) DO UPDATE SET \
                source_agent_id = excluded.source_agent_id, \
                task_goal = COALESCE(excluded.task_goal, session_outcomes.task_goal), \
                turns = excluded.turns, \
                tokens_total = excluded.tokens_total, \
                cost_usd = excluded.cost_usd, \
                wall_clock_secs = excluded.wall_clock_secs, \
                updated_at = excluded.updated_at",
            params![
                outcome_id,
                session_id,
                root_session_id,
                source_agent_id,
                task_goal,
                turns as i64,
                tokens_total as i64,
                cost_usd,
                wall_clock_secs,
                now,
            ],
        )
        .with_context(|| {
            format!(
                "failed to upsert session_outcomes metrics for session {}",
                session_id
            )
        })?;
        Ok(())
    }

    /// Attach a graded `Completion` verdict + evidence to an existing
    /// outcome row.
    ///
    /// Enforces the ownership invariant: `grader_agent_id` must NOT
    /// equal the row's `source_agent_id`. Returns `Err` (and writes
    /// nothing) when the check fails.
    pub fn set_session_outcome_grade(
        &self,
        session_id: &str,
        grader_agent_id: &str,
        completion: Completion,
        evidence_summary: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // Ownership check against the live row. `.optional()` treats
        // QueryReturnedNoRows as `None` but propagates real errors
        // (schema corruption, IO) so they aren't silently swallowed as
        // "row not found".
        let source_agent_id: Option<String> = conn
            .query_row(
                "SELECT source_agent_id FROM session_outcomes WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let source_agent_id = source_agent_id.ok_or_else(|| {
            anyhow::anyhow!(
                "session_outcomes row for session '{}' not found — grade cannot be attached",
                session_id
            )
        })?;
        SessionOutcome::check_grader_ownership(&source_agent_id, grader_agent_id)
            .map_err(|e| anyhow::anyhow!("{}", e))?;

        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE session_outcomes SET \
                completion = ?2, \
                grader_agent_id = ?3, \
                graded_at = ?4, \
                grader_evidence = ?5, \
                updated_at = ?4 \
             WHERE session_id = ?1",
            params![
                session_id,
                completion.as_str(),
                grader_agent_id,
                now,
                evidence_summary,
            ],
        )
        .with_context(|| format!("failed to update session_outcomes grade for {}", session_id))?;
        Ok(())
    }

    /// Attach (or overwrite) an explicit operator rating on an
    /// existing outcome row. Operator rating overrides the grader's
    /// `completion` in the `judged_success` computation but does NOT
    /// rewrite the `completion` column — both signals are preserved
    /// for disagreement analysis.
    pub fn set_session_outcome_operator_rating(
        &self,
        session_id: &str,
        thumb: OperatorThumb,
        note: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let affected = conn.execute(
            "UPDATE session_outcomes SET \
                operator_thumb = ?2, \
                operator_note = ?3, \
                operator_rated_at = ?4, \
                updated_at = ?4 \
             WHERE session_id = ?1",
            params![session_id, thumb.as_str(), note, now],
        )?;
        if affected == 0 {
            return Err(anyhow::anyhow!(
                "no session_outcomes row for session '{}' — cannot attach operator rating",
                session_id
            ));
        }
        Ok(())
    }

    pub fn get_session_outcome(&self, session_id: &str) -> Result<Option<SessionOutcome>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT outcome_id, session_id, root_session_id, source_agent_id, task_goal, \
                    completion, turns, tokens_total, cost_usd, wall_clock_secs, \
                    grader_agent_id, graded_at, grader_evidence, \
                    operator_thumb, operator_note, operator_rated_at, \
                    created_at, updated_at \
             FROM session_outcomes WHERE session_id = ?1",
        )?;
        // `.optional()` distinguishes "no such session" (return None) from
        // real query errors (propagate via `?`). The earlier `.ok()`
        // approach silently swallowed IO / schema errors as "missing row",
        // making operator debugging harder.
        let row = stmt
            .query_row(params![session_id], |row| {
                Ok(SessionOutcomeRecord {
                    outcome_id: row.get(0)?,
                    session_id: row.get(1)?,
                    root_session_id: row.get(2)?,
                    source_agent_id: row.get(3)?,
                    task_goal: row.get(4)?,
                    completion: row.get(5)?,
                    turns: row.get(6)?,
                    tokens_total: row.get(7)?,
                    cost_usd: row.get(8)?,
                    wall_clock_secs: row.get(9)?,
                    grader_agent_id: row.get(10)?,
                    graded_at: row.get(11)?,
                    grader_evidence: row.get(12)?,
                    operator_thumb: row.get(13)?,
                    operator_note: row.get(14)?,
                    operator_rated_at: row.get(15)?,
                    created_at: row.get(16)?,
                    updated_at: row.get(17)?,
                })
            })
            .optional()?;
        Ok(row.map(SessionOutcomeRecord::into_domain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_store() -> (GatewayStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = GatewayStore::open(dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn upsert_creates_then_refreshes_metrics() {
        let (store, _dir) = open_store();
        store
            .upsert_session_outcome_metrics(
                "sess-1", "sess-1", "planner.default", None, 5, 1000, 0.10, 30.0,
            )
            .unwrap();
        let first = store.get_session_outcome("sess-1").unwrap().unwrap();
        assert_eq!(first.turns, 5);
        assert_eq!(first.tokens.total, 1000);
        assert_eq!(first.completion, Completion::Unknown);

        // Same session, larger numbers (the session ran longer).
        store
            .upsert_session_outcome_metrics(
                "sess-1", "sess-1", "planner.default", None, 12, 4200, 0.42, 90.0,
            )
            .unwrap();
        let second = store.get_session_outcome("sess-1").unwrap().unwrap();
        assert_eq!(second.turns, 12);
        assert_eq!(second.tokens.total, 4200);
        // outcome_id is preserved across upserts (UNIQUE on session_id).
        assert_eq!(first.outcome_id, second.outcome_id);
    }

    #[test]
    fn set_grade_enforces_ownership_invariant() {
        let (store, _dir) = open_store();
        store
            .upsert_session_outcome_metrics(
                "sess-2", "sess-2", "planner.default", None, 3, 500, 0.05, 15.0,
            )
            .unwrap();

        // Grader == source agent → must be rejected.
        let err = store
            .set_session_outcome_grade(
                "sess-2",
                "planner.default",
                Completion::Achieved,
                Some("self-grading attempt"),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("Ownership violation"),
            "expected ownership-violation error, got: {}",
            err
        );

        // Confirm nothing was written.
        let row = store.get_session_outcome("sess-2").unwrap().unwrap();
        assert!(row.grader.is_none());
        assert_eq!(row.completion, Completion::Unknown);
    }

    #[test]
    fn set_grade_writes_when_grader_is_independent() {
        let (store, _dir) = open_store();
        store
            .upsert_session_outcome_metrics(
                "sess-3", "sess-3", "planner.default", None, 4, 800, 0.07, 20.0,
            )
            .unwrap();

        store
            .set_session_outcome_grade(
                "sess-3",
                "outcome-grader.default",
                Completion::Achieved,
                Some("planner reached goal in 4 turns"),
            )
            .unwrap();

        let row = store.get_session_outcome("sess-3").unwrap().unwrap();
        assert_eq!(row.completion, Completion::Achieved);
        let grader = row.grader.unwrap();
        assert_eq!(grader.grader_agent_id, "outcome-grader.default");
        assert_eq!(
            grader.evidence_summary.as_deref(),
            Some("planner reached goal in 4 turns")
        );
    }

    #[test]
    fn set_grade_fails_when_session_outcome_missing() {
        let (store, _dir) = open_store();
        let err = store
            .set_session_outcome_grade(
                "no-such-session",
                "outcome-grader.default",
                Completion::Achieved,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn operator_rating_writes_independently_of_grade() {
        let (store, _dir) = open_store();
        store
            .upsert_session_outcome_metrics(
                "sess-4", "sess-4", "planner.default", None, 2, 200, 0.02, 5.0,
            )
            .unwrap();

        store
            .set_session_outcome_operator_rating(
                "sess-4",
                OperatorThumb::Up,
                Some("worked great, fast"),
            )
            .unwrap();

        let row = store.get_session_outcome("sess-4").unwrap().unwrap();
        let rating = row.operator_rating.unwrap();
        assert_eq!(rating.thumb, OperatorThumb::Up);
        assert_eq!(rating.note.as_deref(), Some("worked great, fast"));
        // Completion is untouched — operator rating is an additive overlay,
        // not a grade rewrite.
        assert_eq!(row.completion, Completion::Unknown);
    }

    #[test]
    fn operator_rating_fails_when_session_outcome_missing() {
        let (store, _dir) = open_store();
        let err = store
            .set_session_outcome_operator_rating("no-such-session", OperatorThumb::Up, None)
            .unwrap_err();
        assert!(err.to_string().contains("no session_outcomes row"));
    }

    #[test]
    fn upsert_preserves_overlays_on_refresh() {
        // Once a grade and rating have been attached, re-running the
        // auto-population (e.g., on a session-end retry) must not wipe
        // them. Pins the ON CONFLICT clause's COALESCE behaviour.
        let (store, _dir) = open_store();
        store
            .upsert_session_outcome_metrics(
                "sess-5", "sess-5", "planner.default", None, 3, 300, 0.03, 10.0,
            )
            .unwrap();
        store
            .set_session_outcome_grade(
                "sess-5",
                "outcome-grader.default",
                Completion::Failed,
                Some("missed the goal"),
            )
            .unwrap();
        store
            .set_session_outcome_operator_rating(
                "sess-5",
                OperatorThumb::Down,
                Some("agree"),
            )
            .unwrap();

        // Now re-run the metric upsert (simulates session-end retry).
        store
            .upsert_session_outcome_metrics(
                "sess-5", "sess-5", "planner.default", None, 4, 400, 0.04, 12.0,
            )
            .unwrap();

        let row = store.get_session_outcome("sess-5").unwrap().unwrap();
        // Metrics refreshed:
        assert_eq!(row.turns, 4);
        assert_eq!(row.tokens.total, 400);
        // Overlays preserved:
        assert_eq!(row.completion, Completion::Failed);
        assert!(row.grader.is_some());
        assert_eq!(
            row.operator_rating.as_ref().map(|r| r.thumb),
            Some(OperatorThumb::Down)
        );
    }
}
