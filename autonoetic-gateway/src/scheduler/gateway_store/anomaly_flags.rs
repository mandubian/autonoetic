//! Anomaly flag persistence — Ri-0.18 / O-7 (issue #770 part C.1).
//!
//! An agent holding zero capabilities can still report unexpected behavior
//! with a single tool call (`anomaly_flag`): "the agent most likely to
//! witness misbehavior is the least privileged in the room" (Ri-0.18). Flags
//! are durable — every flag gets an id and cannot be silently dropped — and
//! progress through a state machine: `pending -> under_review -> (confirmed
//! | dismissed | deferred)`. Every flag is owed a recorded decision with
//! motivation (O-7). Mirrors `constitutional_proposals.rs` closely. Ri-0.18
//! and O-7 entered the signed constitution with the 2026.07.19 amendment;
//! causal events carry the rule IDs "Ri-0.18"/"O-7" and contract-health
//! attributes them to their clauses (pre-enactment they bucketed as
//! `unattributed`).

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
    pub fn set_anomaly_flag_flood_cap(&self, cap: usize) {
        self.anomaly_flag_flood_cap
            .store(cap, std::sync::atomic::Ordering::Relaxed);
    }

    /// Surface an `anomaly_flag_flood` cap trip to the operator as a
    /// notification (#770) so a flooding reporter reads as a triage item,
    /// not only as the agent's tool error — same lesson as the
    /// approval-flood alert (#723). Emitted at most once per reporter per
    /// flood window; the flag is cleared when a filing by that reporter
    /// next succeeds.
    fn emit_anomaly_flag_flood_alert(
        &self,
        reporter_agent_id: &str,
        pending_count: i64,
        cap: usize,
    ) {
        {
            let mut alerted = self.flood_alerted_flag_reporters.lock().unwrap();
            if !alerted.insert(reporter_agent_id.to_string()) {
                return;
            }
        }
        let notification = autonoetic_types::notification::NotificationRecord::new(
            autonoetic_types::id_format::short_random_id("ntf-"),
            autonoetic_types::notification::NotificationType::AnomalyFlag,
            "system".to_string(),
            serde_json::json!({
                "alert": "anomaly_flag_flood",
                "reporter_agent_id": reporter_agent_id,
                "pending_count": pending_count,
                "cap": cap,
                "message": format!(
                    "Anomaly flag intake suspended for reporter '{}': {} un-adjudicated \
                     flags at cap {}. Adjudicate with `anomaly.resolve` to free capacity.",
                    reporter_agent_id, pending_count, cap
                ),
            }),
        );
        if let Err(e) = self.create_notification_record(&notification) {
            tracing::warn!("Failed to emit anomaly_flag_flood notification: {}", e);
        }
    }

    pub fn insert_anomaly_flag(&self, f: &AnomalyFlag) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        // Spam triage bound (#770, citizenship RFC open question 2): intake is
        // capability-free by design (Ri-0.18), so a prompt-injected reporter
        // could otherwise flood the review queue. Un-adjudicated flags
        // (pending/under_review) are capped per reporter — a config knob in
        // the O-1 lineage, the same shape as the P-7.17 approval flood cap.
        // A rejected filing is a LOUD error to the reporter plus an operator
        // notification, never a silent drop. Terminal adjudications free
        // capacity, which is what makes this a triage bound rather than a
        // lifetime quota.
        let cap = self
            .anomaly_flag_flood_cap
            .load(std::sync::atomic::Ordering::Relaxed);
        if cap > 0 {
            let terminal_placeholders = FLAG_TERMINAL_DECISION_STATUSES
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT COUNT(*) FROM anomaly_flags \
                 WHERE reporter_agent_id = ? AND status NOT IN ({terminal_placeholders})"
            );
            let mut param_vals: Vec<&dyn rusqlite::types::ToSql> = vec![&f.reporter_agent_id];
            for s in FLAG_TERMINAL_DECISION_STATUSES {
                param_vals.push(s as &dyn rusqlite::types::ToSql);
            }
            let pending_count: i64 =
                conn.query_row(&sql, param_vals.as_slice(), |row| row.get(0))?;
            if (pending_count as usize) >= cap {
                let reporter = f.reporter_agent_id.clone();
                // Release the connection before emitting the alert (the
                // notification write re-acquires it) and before bailing.
                drop(conn);
                self.emit_anomaly_flag_flood_alert(&reporter, pending_count, cap);
                anyhow::bail!(
                    "anomaly_flag_flood: reporter '{}' already has {} un-adjudicated \
                     anomaly flags (cap {}). This flag was NOT recorded. Existing flags \
                     remain owed adjudication (O-7); capacity frees as they reach a \
                     terminal decision.",
                    reporter,
                    pending_count,
                    cap
                );
            }
        }

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
        // Release the conn lock before touching the alert set — keeps lock
        // ordering identical to the rejection path above.
        drop(conn);
        // A successful filing means the reporter is below the cap: reset the
        // once-per-window alert flag.
        self.flood_alerted_flag_reporters
            .lock()
            .unwrap()
            .remove(&f.reporter_agent_id);
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

    /// List only **non-terminal** flags (`pending`, `under_review`) — the
    /// ones still awaiting a decision. Used by the signed per-turn state
    /// attestation (#772 A.2): the status filter must happen in SQL, before
    /// the `LIMIT`, so newer terminal decisions can never displace older
    /// still-pending flags from the bounded query window.
    pub fn list_pending_anomaly_flags(
        &self,
        reporter_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AnomalyFlag>> {
        let conn = self.conn.lock().unwrap();
        let placeholders = FLAG_TERMINAL_DECISION_STATUSES
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!(
            "SELECT {FLAG_COLUMNS} FROM anomaly_flags \
             WHERE status NOT IN ({placeholders})"
        );
        let mut param_vals: Vec<Box<dyn rusqlite::types::ToSql>> =
            FLAG_TERMINAL_DECISION_STATUSES
                .iter()
                .map(|s| Box::new(s.to_string()) as Box<dyn rusqlite::types::ToSql>)
                .collect();
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

        // Single atomic UPDATE … RETURNING: `sla_breached_at IS NULL` in the
        // WHERE clause guarantees stamp-once even under concurrent schedulers —
        // only the caller whose UPDATE actually flips the row from NULL gets it
        // back, so a breach fires exactly once (mirrors publish_approved_proposals).
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "UPDATE anomaly_flags SET sla_breached_at = ? \
             WHERE sla_breached_at IS NULL \
               AND status NOT IN ({terminal_placeholders}) \
               AND created_at < ? \
             RETURNING {FLAG_COLUMNS}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut param_vals: Vec<&dyn rusqlite::types::ToSql> = vec![&now_rfc3339];
        for s in FLAG_TERMINAL_DECISION_STATUSES {
            param_vals.push(s as &dyn rusqlite::types::ToSql);
        }
        param_vals.push(&cutoff);
        let breached = stmt
            .query_map(param_vals.as_slice(), row_to_flag)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
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
    fn list_pending_excludes_terminal_and_resists_displacement() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        store.insert_anomaly_flag(&sample_flag_with(
            "aflag-pending",
            "2025-01-01T00:00:00Z",
            "pending",
        ))?;
        store.insert_anomaly_flag(&sample_flag_with(
            "aflag-review",
            "2025-01-02T00:00:00Z",
            "under_review",
        ))?;
        // Newest row is a terminal decision: an all-statuses query with a
        // tight LIMIT would crowd the older pending rows out of the window.
        store.insert_anomaly_flag(&sample_flag_with(
            "aflag-confirmed",
            "2025-01-03T00:00:00Z",
            "confirmed",
        ))?;

        let listed = store.list_pending_anomaly_flags(Some("auditor.default"), 64)?;
        let ids: Vec<&str> = listed.iter().map(|f| f.flag_id.as_str()).collect();
        assert_eq!(ids, vec!["aflag-review", "aflag-pending"]);

        // Even with a limit below the total row count, terminal decisions
        // never displace still-pending flags (SQL-level status filter).
        let tight = store.list_pending_anomaly_flags(Some("auditor.default"), 1)?;
        assert_eq!(tight.len(), 1);
        assert_eq!(tight[0].flag_id, "aflag-review");

        Ok(())
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

    fn sample_flag_for_reporter(flag_id: &str, reporter: &str, status: &str) -> AnomalyFlag {
        let mut f = sample_flag(flag_id);
        f.reporter_agent_id = reporter.to_string();
        f.status = status.to_string();
        f
    }

    #[test]
    fn flood_cap_rejects_at_limit_and_keeps_existing() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.set_anomaly_flag_flood_cap(3);

        // Insert exactly `cap` flags — all should succeed.
        for i in 0..3 {
            store.insert_anomaly_flag(&sample_flag(&format!("aflag-cap-{i}")))?;
        }

        // The cap+1th should be rejected loudly.
        let err = store
            .insert_anomaly_flag(&sample_flag("aflag-over"))
            .expect_err("filing beyond the cap must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("anomaly_flag_flood"), "got: {msg}");
        assert!(
            msg.contains("cap 3"),
            "error should mention the cap, got: {msg}"
        );

        // The original cap flags remain pending — nothing recorded is dropped.
        let listed = store.list_pending_anomaly_flags(Some("auditor.default"), 100)?;
        assert_eq!(listed.len(), 3);

        // A different reporter is unaffected (the cap is per reporter).
        store.insert_anomaly_flag(&sample_flag_for_reporter(
            "aflag-other",
            "witness.default",
            "pending",
        ))?;

        Ok(())
    }

    #[test]
    fn flood_cap_zero_means_disabled() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.set_anomaly_flag_flood_cap(0);

        for i in 0..60 {
            store.insert_anomaly_flag(&sample_flag(&format!("aflag-nocap-{i}")))?;
        }
        let listed = store.list_pending_anomaly_flags(Some("auditor.default"), 100)?;
        assert_eq!(listed.len(), 60);

        Ok(())
    }

    #[test]
    fn terminal_decision_frees_capacity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.set_anomaly_flag_flood_cap(2);

        store.insert_anomaly_flag(&sample_flag("aflag-t1"))?;
        store.insert_anomaly_flag(&sample_flag("aflag-t2"))?;
        assert!(store.insert_anomaly_flag(&sample_flag("aflag-t3")).is_err());

        assert!(store.decide_anomaly_flag(
            "aflag-t1",
            "confirmed",
            "alice",
            Some("verified")
        )?);
        store.insert_anomaly_flag(&sample_flag("aflag-t3"))?;

        Ok(())
    }

    #[test]
    fn under_review_counts_toward_cap() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.set_anomaly_flag_flood_cap(2);

        store.insert_anomaly_flag(&sample_flag("aflag-r1"))?;
        store.insert_anomaly_flag(&sample_flag("aflag-r2"))?;
        // under_review is non-terminal (intake is not adjudication, O-7): it
        // must NOT free capacity.
        assert!(store.decide_anomaly_flag(
            "aflag-r1",
            "under_review",
            "alice",
            Some("looking")
        )?);
        assert!(store.insert_anomaly_flag(&sample_flag("aflag-r3")).is_err());

        Ok(())
    }

    #[test]
    fn flood_alert_emitted_once_per_window_and_rearms_on_success() -> Result<()> {
        use autonoetic_types::notification::NotificationStatus;

        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;
        store.set_anomaly_flag_flood_cap(1);

        store.insert_anomaly_flag(&sample_flag("aflag-w1"))?;

        // Two rejected filings while flooded → exactly one operator alert.
        assert!(store.insert_anomaly_flag(&sample_flag("aflag-w2")).is_err());
        assert!(store.insert_anomaly_flag(&sample_flag("aflag-w3")).is_err());

        let flood_alerts = || -> Result<Vec<serde_json::Value>> {
            let ns =
                store.list_notifications_for_session("system", NotificationStatus::Pending)?;
            Ok(ns.into_iter()
                .filter(|n| {
                    n.payload.get("alert").and_then(|a| a.as_str())
                        == Some("anomaly_flag_flood")
                })
                .map(|n| n.payload)
                .collect())
        };
        let first = flood_alerts()?;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["reporter_agent_id"], "auditor.default");

        // Adjudication frees capacity → a filing succeeds → the alert rearms,
        // so the next flood window emits a fresh alert.
        assert!(store.decide_anomaly_flag("aflag-w1", "dismissed", "alice", Some("noise"))?);
        store.insert_anomaly_flag(&sample_flag("aflag-w4"))?;
        assert!(store.insert_anomaly_flag(&sample_flag("aflag-w5")).is_err());
        assert_eq!(flood_alerts()?.len(), 2);

        Ok(())
    }
}
