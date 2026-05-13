use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use super::GatewayStore;

/// A recorded post-promotion review for one agent.
#[derive(Debug, Clone, Serialize)]
pub struct PostPromotionReviewRecord {
    pub review_id: String,
    pub agent_id: String,
    pub revision_id: String,
    pub reviewed_at: String,
    pub tool_failures: i64,
    pub auth_denials: i64,
    pub suspensions: i64,
    pub sentinel_findings: i64,
    pub findings_json: String,
}

impl GatewayStore {
    pub fn record_post_promotion_review(
        &self,
        agent_id: &str,
        revision_id: &str,
        reviewed_at: &str,
        tool_failures: i64,
        auth_denials: i64,
        suspensions: i64,
        sentinel_findings: i64,
        findings_json: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO post_promotion_reviews \
             (review_id, agent_id, revision_id, reviewed_at, tool_failures, \
              auth_denials, suspensions, sentinel_findings, findings_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                format!("ppr_{:x}", uuid::Uuid::new_v4().as_u128()),
                agent_id,
                revision_id,
                reviewed_at,
                tool_failures,
                auth_denials,
                suspensions,
                sentinel_findings,
                findings_json,
            ],
        )?;
        Ok(())
    }

    pub fn get_last_post_promotion_review(
        &self,
        agent_id: &str,
    ) -> Result<Option<PostPromotionReviewRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT review_id, agent_id, revision_id, reviewed_at, tool_failures, \
             auth_denials, suspensions, sentinel_findings, findings_json \
             FROM post_promotion_reviews \
             WHERE agent_id = ?1 \
             ORDER BY reviewed_at DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![agent_id], |row| {
            Ok(PostPromotionReviewRecord {
                review_id: row.get(0)?,
                agent_id: row.get(1)?,
                revision_id: row.get(2)?,
                reviewed_at: row.get(3)?,
                tool_failures: row.get(4)?,
                auth_denials: row.get(5)?,
                suspensions: row.get(6)?,
                sentinel_findings: row.get(7)?,
                findings_json: row.get(8)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn get_most_recent_review_timestamp(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result: Option<String> = conn
            .query_row(
                "SELECT MAX(reviewed_at) FROM post_promotion_reviews",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    pub fn list_post_promotion_reviews(
        &self,
        agent_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<PostPromotionReviewRecord>> {
        let conn = self.conn.lock().unwrap();
        let (sql, sql_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            if let Some(aid) = agent_id {
                (
                    "SELECT review_id, agent_id, revision_id, reviewed_at, tool_failures, \
                     auth_denials, suspensions, sentinel_findings, findings_json \
                     FROM post_promotion_reviews WHERE agent_id = ?1 \
                     ORDER BY reviewed_at DESC LIMIT ?2"
                        .to_string(),
                    vec![Box::new(aid.to_string()), Box::new(limit)],
                )
            } else {
                (
                    "SELECT review_id, agent_id, revision_id, reviewed_at, tool_failures, \
                     auth_denials, suspensions, sentinel_findings, findings_json \
                     FROM post_promotion_reviews ORDER BY reviewed_at DESC LIMIT ?1"
                        .to_string(),
                    vec![Box::new(limit)],
                )
            };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            sql_params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(PostPromotionReviewRecord {
                review_id: row.get(0)?,
                agent_id: row.get(1)?,
                revision_id: row.get(2)?,
                reviewed_at: row.get(3)?,
                tool_failures: row.get(4)?,
                auth_denials: row.get(5)?,
                suspensions: row.get(6)?,
                sentinel_findings: row.get(7)?,
                findings_json: row.get(8)?,
            })
        })?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Count tool-failure causal events for an agent since a timestamp.
    pub fn count_tool_failures_since(&self, agent_id: &str, since: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM causal_events \
             WHERE agent_id = ?1 AND category = 'tool' AND status LIKE 'failure%' \
             AND timestamp >= ?2",
            params![agent_id, since],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Count authorization-denial causal events for an agent since a timestamp.
    pub fn count_auth_denials_since(&self, agent_id: &str, since: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM causal_events \
             WHERE agent_id = ?1 AND category = 'approval' AND status LIKE 'denied%' \
             AND timestamp >= ?2",
            params![agent_id, since],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Count session-suspension events for an agent since a timestamp.
    pub fn count_suspensions_since(&self, agent_id: &str, since: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM causal_events \
             WHERE agent_id = ?1 AND category = 'session' AND status LIKE 'suspended%' \
             AND timestamp >= ?2",
            params![agent_id, since],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Count sentinel findings referencing an agent since a timestamp.
    pub fn count_sentinel_findings_for_agent_since(
        &self,
        agent_id: &str,
        since: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM security_findings \
             WHERE created_at >= ?1 AND affected_json LIKE ?2",
            params![since, format!("%{}%", agent_id)],
            |row| row.get(0),
        )?;
        Ok(count)
    }
}
