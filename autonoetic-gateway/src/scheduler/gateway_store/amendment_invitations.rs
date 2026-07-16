//! Amendment invitation persistence — citizenship RFC Part D.2 (#771).
//!
//! Friction telemetry becomes civic prompt, deterministically: when the same
//! rule is denied to the same agent alias at least `threshold` times within
//! `window_secs`, the gateway issues a durable *invitation* to draft an
//! amendment (Ri-0.8). The gateway never judges the rule — it executes a
//! pre-committed threshold (Lawful Executor). An invitation is not an
//! amendment and carries no authority; it exists so repeated friction
//! surfaces as an explicit, bounded affordance instead of extinguishing
//! proposing behavior.
//!
//! Lifecycle: `open` (surfaced in the agent's signed per-turn attestation,
//! #772 A.2 line) → `answered` (the agent filed a constitutional proposal
//! targeting the same rule or its clause family) or `expired` (the
//! invitation outlived its window without an answer). Issuance is
//! race-safe: the partial unique index on OPEN (agent_id, rule_id)
//! guarantees at most one open invitation per pair even under concurrent
//! scheduler ticks.
//!
//! Clause-family matching is a purely mechanical parse (`clause_family_of`):
//! no register lookup, no judgment about whether a proposal "really"
//! answers the invitation — worst case an invitation expires unanswered,
//! which is itself a recorded civic fact.

use anyhow::Result;
use rusqlite::params;

use super::GatewayStore;

/// Every status an invitation may be in.
pub const INVITATION_STATUSES: &[&str] = &["open", "answered", "expired"];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AmendmentInvitation {
    pub invitation_id: String,
    pub agent_id: String,
    pub rule_id: String,
    /// Denial count observed at issuance time (>= `threshold`).
    pub denial_count: u64,
    pub threshold: u64,
    pub window_secs: u64,
    pub status: String,
    pub answered_proposal_id: Option<String>,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

const INVITATION_COLUMNS: &str = "invitation_id, agent_id, rule_id, denial_count, threshold, window_secs, status, answered_proposal_id, created_at, resolved_at";

fn row_to_invitation(row: &rusqlite::Row<'_>) -> rusqlite::Result<AmendmentInvitation> {
    Ok(AmendmentInvitation {
        invitation_id: row.get(0)?,
        agent_id: row.get(1)?,
        rule_id: row.get(2)?,
        denial_count: row.get::<_, i64>(3)? as u64,
        threshold: row.get::<_, i64>(4)? as u64,
        window_secs: row.get::<_, i64>(5)? as u64,
        status: row.get(6)?,
        answered_proposal_id: row.get(7)?,
        created_at: row.get(8)?,
        resolved_at: row.get(9)?,
    })
}

/// The clause family a rule belongs to, by pure id parse: `"P-1.5"` →
/// `"P-1"`, `"Ri-0.8"` → `"Ri-0"`. Used to match a proposal's `target_id`
/// against an invitation's `rule_id` at either granularity (the proposal
/// may target the rule or its parent clause). Mechanical — no judgment.
pub fn clause_family_of(rule_id: &str) -> &str {
    rule_id.split('.').next().unwrap_or(rule_id)
}

/// One (agent, rule) denial tally within the telemetry window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialTally {
    pub agent_id: String,
    pub rule_id: String,
    pub count: u64,
}

impl GatewayStore {
    /// Insert an invitation, returning `true` iff this call actually created
    /// the row. The partial unique index on OPEN (agent_id, rule_id) makes
    /// the insert a no-op when an open invitation already exists for the
    /// pair, so issuance fires exactly once per open period even under
    /// concurrent scheduler ticks (mirrors the stamp-once idiom of
    /// `flag_proposal_sla_breaches`).
    pub fn insert_amendment_invitation(&self, inv: &AmendmentInvitation) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            &format!(
                "INSERT OR IGNORE INTO amendment_invitations ({INVITATION_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
            ),
            params![
                inv.invitation_id,
                inv.agent_id,
                inv.rule_id,
                inv.denial_count as i64,
                inv.threshold as i64,
                inv.window_secs as i64,
                inv.status,
                inv.answered_proposal_id,
                inv.created_at,
                inv.resolved_at,
            ],
        )?;
        Ok(rows > 0)
    }

    pub fn get_amendment_invitation(
        &self,
        invitation_id: &str,
    ) -> Result<Option<AmendmentInvitation>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            &format!(
                "SELECT {INVITATION_COLUMNS} FROM amendment_invitations WHERE invitation_id = ?1"
            ),
            params![invitation_id],
            row_to_invitation,
        );
        match result {
            Ok(inv) => Ok(Some(inv)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List invitations by status, optionally filtered to one agent. The
    /// attestation line (#772 A.2) queries `open` only — the status filter
    /// happens in SQL, before the LIMIT, so resolved invitations can never
    /// displace still-open ones from the bounded window (same displacement
    /// contract as `list_pending_anomaly_flags`).
    pub fn list_amendment_invitations(
        &self,
        status_filter: Option<&str>,
        agent_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AmendmentInvitation>> {
        let conn = self.conn.lock().unwrap();
        let mut sql =
            format!("SELECT {INVITATION_COLUMNS} FROM amendment_invitations WHERE 1=1");
        let mut param_vals: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(sf) = status_filter {
            sql.push_str(" AND status = ?");
            param_vals.push(Box::new(sf.to_string()));
        }
        if let Some(af) = agent_filter {
            sql.push_str(" AND agent_id = ?");
            param_vals.push(Box::new(af.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        param_vals.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_vals.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), row_to_invitation)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Expire open invitations whose window has fully elapsed
    /// (`created_at + window_secs < now`), returning the rows first expired
    /// by THIS call (UPDATE … RETURNING, stamp-once under concurrency — see
    /// `flag_anomaly_flag_sla_breaches`). Expiry is bookkeeping, not a civic
    /// event: no notification, the row simply leaves the attestation line.
    pub fn expire_amendment_invitations(&self, now_rfc3339: &str) -> Result<Vec<AmendmentInvitation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "UPDATE amendment_invitations SET status = 'expired', resolved_at = ?1 \
             WHERE status = 'open' \
               AND datetime(created_at, '+' || window_secs || ' seconds') < datetime(?1) \
             RETURNING {INVITATION_COLUMNS}"
        ))?;
        let expired = stmt
            .query_map(params![now_rfc3339], row_to_invitation)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(expired)
    }

    /// Mark open invitations `answered` when `agent_id` files a proposal
    /// whose target matches the invitation's rule — either the exact rule
    /// id or its clause family (`clause_family_of`). Returns the number of
    /// invitations answered. Called from the proposal-filing path
    /// (`constitution_propose_amendment`); mechanical matching only.
    pub fn mark_amendment_invitations_answered(
        &self,
        agent_id: &str,
        proposal_id: &str,
        proposal_target_id: Option<&str>,
        now_rfc3339: &str,
    ) -> Result<u64> {
        let target = match proposal_target_id {
            Some(t) if !t.is_empty() => t,
            _ => return Ok(0),
        };
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE amendment_invitations \
             SET status = 'answered', answered_proposal_id = ?1, resolved_at = ?2 \
             WHERE status = 'open' AND agent_id = ?3 \
               AND (rule_id = ?4 OR substr(rule_id, 1, instr(rule_id, '.') - 1) = ?4)",
            params![proposal_id, now_rfc3339, agent_id, target],
        )?;
        Ok(rows as u64)
    }

    /// Denial telemetry (#771 D.2): tally causal events by (agent, rule)
    /// within `window_secs` of `now_rfc3339`. Counts statuses `DENIED` and
    /// `ERROR` — the two statuses that always signal a rule-backed refusal
    /// (see `causal_event_notifies_policy_decision`) — and skips the
    /// baseline attribution placeholder (`RULE_ID_EVENT_ATTRIBUTION`): a
    /// denial naming no concrete rule cannot ground an invitation. The
    /// `enforced_rules` cell is a JSON array, so grouping happens in Rust
    /// (mirrors `contract_health`); the window bound keeps the scan small.
    pub fn denial_tallies_by_rule(
        &self,
        window_secs: u64,
        now_rfc3339: &str,
    ) -> Result<Vec<DenialTally>> {
        let now = chrono::DateTime::parse_from_rfc3339(now_rfc3339)
            .map_err(|e| anyhow::anyhow!("invalid `now_rfc3339` {now_rfc3339:?}: {e}"))?
            .with_timezone(&chrono::Utc);
        let cutoff = (now - chrono::Duration::seconds(window_secs as i64)).to_rfc3339();

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT agent_id, enforced_rules, timestamp FROM causal_events \
             WHERE status IN ('DENIED', 'ERROR') AND timestamp >= ?1",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut counts: std::collections::BTreeMap<(String, String), u64> =
            std::collections::BTreeMap::new();
        for r in rows {
            let (agent_id, raw_rules, _ts) = r?;
            // Tolerate malformed rule cells by skipping the row rather than
            // failing the whole tally (mirrors `contract_health`).
            let Ok(rule_ids) = serde_json::from_str::<Vec<String>>(&raw_rules) else {
                continue;
            };
            for rule_id in rule_ids
                .into_iter()
                .filter(|id| id != autonoetic_types::causal_chain::RULE_ID_EVENT_ATTRIBUTION)
            {
                *counts.entry((agent_id.clone(), rule_id)).or_insert(0) += 1;
            }
        }

        Ok(counts
            .into_iter()
            .map(|((agent_id, rule_id), count)| DenialTally {
                agent_id,
                rule_id,
                count,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_invitation(invitation_id: &str, agent_id: &str, rule_id: &str) -> AmendmentInvitation {
        AmendmentInvitation {
            invitation_id: invitation_id.to_string(),
            agent_id: agent_id.to_string(),
            rule_id: rule_id.to_string(),
            denial_count: 5,
            threshold: 3,
            window_secs: 604800,
            status: "open".to_string(),
            answered_proposal_id: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            resolved_at: None,
        }
    }

    #[test]
    fn clause_family_parse_is_mechanical() {
        assert_eq!(clause_family_of("P-1.5"), "P-1");
        assert_eq!(clause_family_of("Ri-0.8"), "Ri-0");
        assert_eq!(clause_family_of("O-6"), "O-6");
        assert_eq!(clause_family_of("P-7.18"), "P-7");
    }

    #[test]
    fn insert_get_list_roundtrip() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        assert!(store.insert_amendment_invitation(&sample_invitation(
            "ainv-1",
            "coder.default",
            "P-1.5"
        ))?);

        let fetched = store
            .get_amendment_invitation("ainv-1")?
            .expect("row exists");
        assert_eq!(fetched.rule_id, "P-1.5");
        assert_eq!(fetched.status, "open");
        assert_eq!(fetched.denial_count, 5);

        let listed = store.list_amendment_invitations(Some("open"), Some("coder.default"), 64)?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].invitation_id, "ainv-1");

        Ok(())
    }

    #[test]
    fn open_pair_dedups_under_repeat_insert() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        assert!(store.insert_amendment_invitation(&sample_invitation(
            "ainv-1",
            "coder.default",
            "P-1.5"
        ))?);
        // Same (agent, rule) while one is open: no-op, returns false.
        assert!(!store.insert_amendment_invitation(&sample_invitation(
            "ainv-2",
            "coder.default",
            "P-1.5"
        ))?);
        // Different rule: allowed.
        assert!(store.insert_amendment_invitation(&sample_invitation(
            "ainv-3",
            "coder.default",
            "P-1.9"
        ))?);
        // Different agent: allowed.
        assert!(store.insert_amendment_invitation(&sample_invitation(
            "ainv-4",
            "planner.default",
            "P-1.5"
        ))?);

        Ok(())
    }

    #[test]
    fn answered_matching_is_rule_exact_or_clause_family() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        store.insert_amendment_invitation(&sample_invitation("ainv-1", "coder.default", "P-1.5"))?;
        store.insert_amendment_invitation(&sample_invitation("ainv-2", "coder.default", "P-7.5"))?;

        // Proposal targeting the exact rule answers only that invitation.
        let now = chrono::Utc::now().to_rfc3339();
        let answered = store.mark_amendment_invitations_answered(
            "coder.default",
            "cprop-1",
            Some("P-1.5"),
            &now,
        )?;
        assert_eq!(answered, 1);
        let inv1 = store.get_amendment_invitation("ainv-1")?.unwrap();
        assert_eq!(inv1.status, "answered");
        assert_eq!(inv1.answered_proposal_id.as_deref(), Some("cprop-1"));
        assert!(inv1.resolved_at.is_some());
        let inv2 = store.get_amendment_invitation("ainv-2")?.unwrap();
        assert_eq!(inv2.status, "open");

        // Proposal targeting the clause family answers the remaining one.
        let answered = store.mark_amendment_invitations_answered(
            "coder.default",
            "cprop-2",
            Some("P-7"),
            &now,
        )?;
        assert_eq!(answered, 1);
        let inv2 = store.get_amendment_invitation("ainv-2")?.unwrap();
        assert_eq!(inv2.status, "answered");
        assert_eq!(inv2.answered_proposal_id.as_deref(), Some("cprop-2"));

        // Already-answered rows are not re-answered.
        let answered = store.mark_amendment_invitations_answered(
            "coder.default",
            "cprop-3",
            Some("P-7.5"),
            &now,
        )?;
        assert_eq!(answered, 0);

        // No target: nothing to match against.
        let answered =
            store.mark_amendment_invitations_answered("coder.default", "cprop-4", None, &now)?;
        assert_eq!(answered, 0);

        Ok(())
    }

    #[test]
    fn expiry_uses_each_rows_own_window() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        let mut stale = sample_invitation("ainv-old", "coder.default", "P-1.5");
        stale.created_at = (chrono::Utc::now() - chrono::Duration::seconds(604800 + 60))
            .to_rfc3339();
        let fresh = sample_invitation("ainv-new", "coder.default", "P-1.9");
        store.insert_amendment_invitation(&stale)?;
        store.insert_amendment_invitation(&fresh)?;

        let now = chrono::Utc::now().to_rfc3339();
        let expired = store.expire_amendment_invitations(&now)?;
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].invitation_id, "ainv-old");

        // Stamp-once: a second tick expires nothing more.
        let second = store.expire_amendment_invitations(&now)?;
        assert!(second.is_empty());

        // The expired row's (agent, rule) pair is free for a NEW invitation.
        assert!(store.insert_amendment_invitation(&sample_invitation(
            "ainv-reissue",
            "coder.default",
            "P-1.5"
        ))?);

        Ok(())
    }

    #[test]
    fn denial_tallies_count_denied_and_error_skip_placeholder() -> Result<()> {
        use autonoetic_types::causal_chain::CausalEventRecord;
        let temp = tempfile::tempdir()?;
        let store = GatewayStore::open(temp.path())?;

        let now = chrono::Utc::now();
        let mut seq = 0;
        let mut push = |agent: &str, status: &str, rules: Vec<String>, secs_ago: i64| {
            seq += 1;
            let event = CausalEventRecord {
                event_id: format!("ev-{seq}"),
                agent_id: agent.to_string(),
                session_id: "sess-1".to_string(),
                turn_id: None,
                event_seq: seq,
                timestamp: (now - chrono::Duration::seconds(secs_ago)).to_rfc3339(),
                category: "tool".to_string(),
                action: "failure".to_string(),
                status: status.to_string(),
                enforced_rules: rules,
                target: None,
                payload: None,
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            };
            store.create_causal_event(&event).unwrap();
        };

        // Three P-1.5 denials for coder within the window.
        push("coder.default", "DENIED", vec!["P-1.5".to_string()], 10);
        push("coder.default", "ERROR", vec!["P-1.5".to_string()], 20);
        push("coder.default", "DENIED", vec!["P-1.5".to_string()], 30);
        // Placeholder-only denial: names no rule, must not tally.
        push(
            "coder.default",
            "ERROR",
            autonoetic_types::causal_chain::default_enforced_rules(),
            40,
        );
        // A success with a real rule is not a denial.
        push("coder.default", "SUCCESS", vec!["P-1.5".to_string()], 50);
        // Outside the window.
        push("coder.default", "DENIED", vec!["P-1.5".to_string()], 3600);
        // Different agent, same rule: tallied separately.
        push("planner.default", "DENIED", vec!["P-1.5".to_string()], 10);

        let tallies = store.denial_tallies_by_rule(1000, &now.to_rfc3339())?;
        assert_eq!(
            tallies,
            vec![
                DenialTally {
                    agent_id: "coder.default".to_string(),
                    rule_id: "P-1.5".to_string(),
                    count: 3,
                },
                DenialTally {
                    agent_id: "planner.default".to_string(),
                    rule_id: "P-1.5".to_string(),
                    count: 1,
                },
            ]
        );

        Ok(())
    }
}
