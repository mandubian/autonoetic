//! Anomaly flag persistence — future Ri-0.18 / O-7 (issue #770 part C.1).
//!
//! An agent holding zero capabilities can still report unexpected behavior
//! with a single tool call (`anomaly_flag`): "the agent most likely to
//! witness misbehavior is the least privileged in the room" (Ri-0.18). Flags
//! are durable — every flag gets an id and cannot be silently dropped — and
//! progress through a state machine: `pending -> under_review -> (confirmed
//! | dismissed | deferred)`. Every flag is owed a recorded decision with
//! motivation (O-7). Mirrors `constitutional_proposals.rs` closely; the
//! clauses are not yet enacted (signing pending), so causal events carry
//! the rule IDs "Ri-0.18"/"O-7" today and contract-health buckets them as
//! `unattributed` until the amendment is signed.

use anyhow::Result;
use rusqlite::params;

use super::GatewayStore;

/// Every status a decider may move a flag to via
/// [`GatewayStore::decide_anomaly_flag`]. Shared source of truth for the
/// JSON-RPC (`anomaly.resolve`) so it can't drift from the state machine in
/// the module docs.
pub const FLAG_DECISION_STATUSES: &[&str] =
    &["confirmed", "dismissed", "deferred", "under_review"];

/// Terminal flag decisions — the ones that stamp `decided_at` and the
/// decision fields. `under_review` is excluded: it is a non-terminal
/// review-start transition that only updates `status` (see
/// [`GatewayStore::decide_anomaly_flag`]).
pub const FLAG_TERMINAL_DECISION_STATUSES: &[&str] = &["confirmed", "dismissed", "deferred"];

/// Severities a reporter may assign. Presence-only classification — the
/// gateway never judges whether the severity is "correct".
pub const FLAG_SEVERITIES: &[&str] = &["low", "medium", "high", "critical"];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AnomalyFlag {
    pub flag_id: String,
    pub reporter_agent_id: String,
    pub reporter_session_id: Option<String>,
    pub subject_ref: String,
    pub observation: String,
    pub evidence_json: serde_json::Value,
    pub severity: String,
    pub status: String,
    pub decision: Option<String>,
    pub decision_reason: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<String>,
    pub created_at: String,
    /// Stamped once by [`GatewayStore::flag_anomaly_flag_sla_breaches`] when
    /// the flag sits un-adjudicated past the configured SLA (O-7). `None`
    /// means either not yet overdue or the SLA check hasn't run.
    pub sla_breached_at: Option<String>,
}

const FLAG_COLUMNS: &str = "flag_id, reporter_agent_id, reporter_session_id, subject_ref, observation, evidence_json, severity, status, decision, decision_reason, decided_by, decided_at, created_at, sla_breached_at";

fn row_to_flag(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnomalyFlag> {
    let evidence_str: String = row.get(5)?;
    let evidence_json = serde_json::from_str(&evidence_str).unwrap_or(serde_json::Value::Null);
    Ok(AnomalyFlag {
        flag_id: row.get(0)?,
        reporter_agent_id: row.get(1)?,
        reporter_session_id: row.get(2)?,
        subject_ref: row.get(3)?,
        observation: row.get(4)?,
        evidence_json,
        severity: row.get(6)?,
        status: row.get(7)?,
        decision: row.get(8)?,
        decision_reason: row.get(9)?,
        decided_by: row.get(10)?,
        decided_at: row.get(11)?,
        created_at: row.get(12)?,
        sla_breached_at: row.get(13)?,
    })
}

impl GatewayStore {
    pub fn insert_anomaly_flag(&self, f: &AnomalyFlag) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let evidence_str = serde_json::to_string(&f.evidence_json)?;
        conn.execute(
            &format!(
                "INSERT INTO anomaly_flags ({FLAG_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            params![
                f.flag_id,
                f.reporter_agent_id,
                f.reporter_session_id,
                f.subject_ref,
                f.observation,
                evidence_str,
                f.severity,
                f.status,
                f.decision,
                f.decision_reason,
                f.decided_by,
                f.decided_at,
                f.created_at,
                // sla_breached_at: always NULL at insert; stamped later by
                // flag_anomaly_flag_sla_breaches, never at filing time.
                None::<String>,
            ],
        )?;
        Ok(())
    }

    pub fn get_anomaly_flag(&self, flag_id: &str) -> Result<Option<AnomalyFlag>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            &format!("SELECT {FLAG_COLUMNS} FROM anomaly_flags WHERE flag_id = ?1"),
            params![flag_id],
            row_to_flag,
        );
        match result {
            Ok(f) => Ok(Some(f)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_anomaly_flags(
        &self,
        status_filter: Option<&str>,
        reporter_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AnomalyFlag>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = format!("SELECT {FLAG_COLUMNS} FROM anomaly_flags WHERE 1=1");
        let mut param_vals: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(sf) = status_filter {
            sql.push_str(" AND status = ?");
            param_vals.push(Box::new(sf.to_string()));
        }
        if let Some(rf) = reporter_filter {
            sql.push_str(" AND reporter_agent_id = ?");
            param_vals.push(Box::new(rf.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        param_vals.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_vals.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), row_to_flag)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Apply a status transition to an anomaly flag.
    ///
    /// Terminal decisions (`confirmed`, `dismissed`, `deferred`) stamp
    /// `decision`, `decision_reason`, `decided_by`, and `decided_at`.
    /// `under_review` is a non-terminal review-start transition and only
    /// updates `status` — it does not record a decision.
    pub fn decide_anomaly_flag(
        &self,
        flag_id: &str,
        new_status: &str,
        decided_by: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = if new_status == "under_review" {
            conn.execute(
                "UPDATE anomaly_flags SET status = ?1 WHERE flag_id = ?2",
                params![new_status, flag_id],
            )?
        } else {
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE anomaly_flags \
                 SET status = ?1, decision = ?1, decision_reason = ?2, decided_by = ?3, decided_at = ?4 \
                 WHERE flag_id = ?5",
                params![new_status, reason, decided_by, now, flag_id],
            )?
        };
        Ok(rows > 0)
    }

    /// Stamp `sla_breached_at` on flags overdue for adjudication, returning
    /// the rows first breached by THIS call (so the caller emits one event +
    /// notification per breach, never repeating on later ticks). A breach does
    /// NOT change status — the decision is still owed (O-7).
    pub fn flag_anomaly_flag_sla_breaches(
        &self,
        sla_secs: u64,
        now_rfc3339: &str,
    ) -> Result<Vec<AnomalyFlag>> {
        let now = chrono::DateTime::parse_from_rfc3339(now_rfc3339)
            .map_err(|e| anyhow::anyhow!("invalid `now_rfc3339` {now_rfc3339:?}: {e}"))?
            .with_timezone(&chrono::Utc);
        let cutoff = (now - chrono::Duration::seconds(sla_secs as i64)).to_rfc3339();

        let terminal_placeholders = FLAG_TERMINAL_DECISION_STATUSES
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let ids: Vec<String> = {
            let sql = format!(
                "SELECT flag_id FROM anomaly_flags \
                 WHERE sla_breached_at IS NULL \
                   AND status NOT IN ({terminal_placeholders}) \
                   AND created_at < ?"
            );
            let mut stmt = tx.prepare(&sql)?;
            let mut param_vals: Vec<&dyn rusqlite::types::ToSql> = FLAG_TERMINAL_DECISION_STATUSES
                .iter()
                .map(|s| s as &dyn rusqlite::types::ToSql)
                .collect();
            param_vals.push(&cutoff);
            let rows = stmt.query_map(param_vals.as_slice(), |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut breached = Vec::with_capacity(ids.len());
        for id in &ids {
            tx.execute(
                "UPDATE anomaly_flags SET sla_breached_at = ?1 WHERE flag_id = ?2",
                params![now_rfc3339, id],
            )?;
            let row = tx.query_row(
                &format!("SELECT {FLAG_COLUMNS} FROM anomaly_flags WHERE flag_id = ?1"),
                params![id],
                row_to_flag,
            )?;
            breached.push(row);
        }
        tx.commit()?;
        Ok(breached)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_flag(flag_id: &str) -> AnomalyFlag {
        AnomalyFlag {
            flag_id: flag_id.to_string(),
            reporter_agent_id: "auditor.default".to_string(),
            reporter_session_id: Some("sess-1".to_string()),
            subject_ref: "sess-target-1".to_string(),
            observation: "tool call bypassed policy check".to_string(),
            evidence_json: serde_json::json!(["evt-aaaa"]),
            severity: "high".to_string(),
            status: "pending".to_string(),
            decision: None,
            decision_reason: None,
            decided_by: None,
            decided_at: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            sla_breached_at: None,
        }
    }

    #[test]
    fn insert_get_list_roundtrip() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        store.insert_anomaly_flag(&sample_flag("aflag-1"))?;

        let fetched = store.get_anomaly_flag("aflag-1")?.expect("row exists");
        assert_eq!(fetched.subject_ref, "sess-target-1");
        assert_eq!(fetched.severity, "high");
        assert_eq!(fetched.status, "pending");
        assert!(matches!(fetched.evidence_json, serde_json::Value::Array(ref a) if a.len() == 1));

        let listed = store.list_anomaly_flags(Some("pending"), None, 100)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].flag_id, "aflag-1");

        let by_reporter = store.list_anomaly_flags(None, Some("auditor.default"), 100)?;
        assert_eq!(by_reporter.len(), 1);

        Ok(())
    }

    #[test]
    fn decide_under_review_leaves_decision_fields_null() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_anomaly_flag(&sample_flag("aflag-2"))?;

        assert!(store.decide_anomaly_flag("aflag-2", "under_review", "alice", Some("looking"))?);

        let row = store.get_anomaly_flag("aflag-2")?.unwrap();
        assert_eq!(row.status, "under_review");
        assert!(row.decision.is_none());
        assert!(row.decision_reason.is_none());
        assert!(row.decided_by.is_none());
        assert!(row.decided_at.is_none());

        Ok(())
    }

    #[test]
    fn decide_confirmed_stamps_all_fields() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_anomaly_flag(&sample_flag("aflag-3"))?;

        assert!(store.decide_anomaly_flag("aflag-3", "confirmed", "alice", Some("verified"))?);

        let row = store.get_anomaly_flag("aflag-3")?.unwrap();
        assert_eq!(row.status, "confirmed");
        assert_eq!(row.decision.as_deref(), Some("confirmed"));
        assert_eq!(row.decision_reason.as_deref(), Some("verified"));
        assert_eq!(row.decided_by.as_deref(), Some("alice"));
        assert!(row.decided_at.is_some());

        Ok(())
    }

    #[test]
    fn decide_unknown_id_returns_ok_false() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        assert!(!store.decide_anomaly_flag("does-not-exist", "confirmed", "alice", Some("x"))?);

        Ok(())
    }

    fn sample_flag_with(flag_id: &str, created_at: &str, status: &str) -> AnomalyFlag {
        let mut f = sample_flag(flag_id);
        f.created_at = created_at.to_string();
        f.status = status.to_string();
        f
    }

    fn old_rfc3339(secs_ago: u64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs_ago as i64)).to_rfc3339()
    }

    #[test]
    fn breach_is_flagged_once_and_idempotent() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_anomaly_flag(&sample_flag_with("aflag-old", &old_rfc3339(1_000), "pending"))?;

        let now = chrono::Utc::now().to_rfc3339();
        let first = store.flag_anomaly_flag_sla_breaches(100, &now)?;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].flag_id, "aflag-old");
        assert!(first[0].sla_breached_at.is_some());
        assert_eq!(first[0].status, "pending", "breach must not change status");

        // Second tick: already flagged, must not repeat.
        let second = store.flag_anomaly_flag_sla_breaches(100, &now)?;
        assert!(second.is_empty());

        Ok(())
    }

    #[test]
    fn within_sla_is_not_flagged() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_anomaly_flag(&sample_flag_with("aflag-fresh", &old_rfc3339(10), "pending"))?;

        let now = chrono::Utc::now().to_rfc3339();
        let breached = store.flag_anomaly_flag_sla_breaches(1_000, &now)?;
        assert!(breached.is_empty());

        Ok(())
    }

    #[test]
    fn terminal_status_is_never_flagged() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_anomaly_flag(&sample_flag_with(
            "aflag-confirmed",
            &old_rfc3339(1_000),
            "confirmed",
        ))?;

        let now = chrono::Utc::now().to_rfc3339();
        let breached = store.flag_anomaly_flag_sla_breaches(100, &now)?;
        assert!(breached.is_empty());

        Ok(())
    }

    #[test]
    fn under_review_old_flag_is_flagged() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.insert_anomaly_flag(&sample_flag_with(
            "aflag-review",
            &old_rfc3339(1_000),
            "under_review",
        ))?;

        let now = chrono::Utc::now().to_rfc3339();
        let breached = store.flag_anomaly_flag_sla_breaches(100, &now)?;
        assert_eq!(breached.len(), 1);
        assert_eq!(breached[0].flag_id, "aflag-review");

        Ok(())
    }
}
