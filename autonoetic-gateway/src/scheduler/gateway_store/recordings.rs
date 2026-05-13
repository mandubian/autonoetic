use anyhow::{bail, Result};
use autonoetic_types::recording::{FixtureSet, FixtureSetStatus, RecordingSession, RecordingStatus};
use rusqlite::{params, OptionalExtension};

use super::GatewayStore;

fn row_to_recording_session(row: &rusqlite::Row) -> rusqlite::Result<RecordingSession> {
    let status_str: String = row.get(12)?;
    let status = RecordingStatus::parse(&status_str).unwrap_or(RecordingStatus::Active);
    Ok(RecordingSession {
        session_id: row.get(0)?,
        agent_id: row.get(1)?,
        artifact_id: row.get(2)?,
        revision_id: row.get(3)?,
        root_session_id: row.get(4)?,
        started_at: row.get(5)?,
        stopped_at: row.get(6)?,
        duration_secs: row.get(7)?,
        max_requests: row.get(8)?,
        max_bytes: row.get(9)?,
        request_count: row.get::<_, i64>(10)?,
        total_bytes: row.get::<_, i64>(11)?,
        status,
        fixture_set_id: row.get(13)?,
        created_by: row.get(14)?,
    })
}

fn row_to_fixture_set(row: &rusqlite::Row) -> rusqlite::Result<FixtureSet> {
    let status_str: String = row.get(11)?;
    let status = FixtureSetStatus::parse(&status_str).unwrap_or(FixtureSetStatus::Ready);
    Ok(FixtureSet {
        fixture_set_id: row.get(0)?,
        agent_id: row.get(1)?,
        revision_id: row.get(2)?,
        recording_session_id: row.get(3)?,
        created_at: row.get(4)?,
        fixture_file_count: row.get::<_, i64>(5)?,
        total_bytes: row.get::<_, i64>(6)?,
        digest: row.get(7)?,
        host_summary: serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default(),
        host_count: row.get::<_, i64>(9)?,
        redaction_summary: serde_json::from_str(&row.get::<_, String>(10)?).unwrap_or_default(),
        status,
    })
}

impl GatewayStore {
    pub fn create_recording_session(&self, session: &RecordingSession) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO recording_sessions \
             (session_id, agent_id, artifact_id, revision_id, root_session_id, \
              started_at, stopped_at, duration_secs, max_requests, max_bytes, \
              request_count, total_bytes, status, fixture_set_id, created_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                session.session_id,
                session.agent_id,
                session.artifact_id,
                session.revision_id,
                session.root_session_id,
                session.started_at,
                session.stopped_at,
                session.duration_secs,
                session.max_requests,
                session.max_bytes,
                session.request_count,
                session.total_bytes,
                session.status.as_str(),
                session.fixture_set_id,
                session.created_by,
            ],
        )?;
        Ok(())
    }

    pub fn get_recording_session(&self, session_id: &str) -> Result<Option<RecordingSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT session_id, agent_id, artifact_id, revision_id, root_session_id, \
             started_at, stopped_at, duration_secs, max_requests, max_bytes, \
             request_count, total_bytes, status, fixture_set_id, created_by \
             FROM recording_sessions WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![session_id], row_to_recording_session)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_recording_sessions(
        &self,
        agent_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<RecordingSession>> {
        let conn = self.conn.lock().unwrap();
        let (sql, sql_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            if let Some(aid) = agent_id {
                (
                    "SELECT session_id, agent_id, artifact_id, revision_id, root_session_id, \
                     started_at, stopped_at, duration_secs, max_requests, max_bytes, \
                     request_count, total_bytes, status, fixture_set_id, created_by \
                     FROM recording_sessions WHERE agent_id = ?1 ORDER BY started_at DESC LIMIT ?2"
                        .to_string(),
                    vec![Box::new(aid.to_string()), Box::new(limit)],
                )
            } else {
                (
                    "SELECT session_id, agent_id, artifact_id, revision_id, root_session_id, \
                     started_at, stopped_at, duration_secs, max_requests, max_bytes, \
                     request_count, total_bytes, status, fixture_set_id, created_by \
                     FROM recording_sessions ORDER BY started_at DESC LIMIT ?1"
                        .to_string(),
                    vec![Box::new(limit)],
                )
            };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            sql_params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), row_to_recording_session)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn update_recording_session_request_count(
        &self,
        session_id: &str,
        request_count: i64,
        total_bytes: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE recording_sessions SET request_count = ?1, total_bytes = ?2 \
             WHERE session_id = ?3",
            params![request_count, total_bytes, session_id],
        )?;
        Ok(())
    }

    pub fn stop_recording_session(&self, session_id: &str, status: RecordingStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let current_status: String = conn
            .query_row(
                "SELECT status FROM recording_sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("Recording session '{}' not found", session_id))?;

        if current_status != "active" {
            bail!(
                "Recording session '{}' is already '{}'; cannot stop again",
                session_id,
                current_status
            );
        }

        conn.execute(
            "UPDATE recording_sessions SET status = ?1, stopped_at = ?2 WHERE session_id = ?3",
            params![status.as_str(), chrono::Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    pub fn delete_recording_session(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM recording_sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(deleted > 0)
    }

    pub fn set_recording_session_fixture_set(
        &self,
        session_id: &str,
        fixture_set_id: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE recording_sessions SET fixture_set_id = ?1 WHERE session_id = ?2",
            params![fixture_set_id, session_id],
        )?;
        Ok(())
    }

    pub fn create_fixture_set(&self, fixture_set: &FixtureSet) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fixture_sets \
             (fixture_set_id, agent_id, revision_id, recording_session_id, created_at, \
              fixture_file_count, total_bytes, digest, host_summary, host_count, \
              redaction_summary, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                fixture_set.fixture_set_id,
                fixture_set.agent_id,
                fixture_set.revision_id,
                fixture_set.recording_session_id,
                fixture_set.created_at,
                fixture_set.fixture_file_count,
                fixture_set.total_bytes,
                fixture_set.digest,
                serde_json::to_string(&fixture_set.host_summary)?,
                fixture_set.host_count,
                serde_json::to_string(&fixture_set.redaction_summary)?,
                fixture_set.status.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn get_fixture_set(&self, fixture_set_id: &str) -> Result<Option<FixtureSet>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT fixture_set_id, agent_id, revision_id, recording_session_id, created_at, \
             fixture_file_count, total_bytes, digest, host_summary, host_count, \
             redaction_summary, status \
             FROM fixture_sets WHERE fixture_set_id = ?1",
        )?;
        let mut rows = stmt.query_map(params![fixture_set_id], row_to_fixture_set)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_fixture_sets(
        &self,
        agent_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<FixtureSet>> {
        let conn = self.conn.lock().unwrap();
        let (sql, sql_params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
            if let Some(aid) = agent_id {
                (
                    "SELECT fixture_set_id, agent_id, revision_id, recording_session_id, created_at, \
                     fixture_file_count, total_bytes, digest, host_summary, host_count, \
                     redaction_summary, status \
                     FROM fixture_sets WHERE agent_id = ?1 ORDER BY created_at DESC LIMIT ?2"
                        .to_string(),
                    vec![Box::new(aid.to_string()), Box::new(limit)],
                )
            } else {
                (
                    "SELECT fixture_set_id, agent_id, revision_id, recording_session_id, created_at, \
                     fixture_file_count, total_bytes, digest, host_summary, host_count, \
                     redaction_summary, status \
                     FROM fixture_sets ORDER BY created_at DESC LIMIT ?1"
                        .to_string(),
                    vec![Box::new(limit)],
                )
            };
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            sql_params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), row_to_fixture_set)?;
        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    pub fn delete_fixture_set(&self, fixture_set_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM fixture_sets WHERE fixture_set_id = ?1",
            params![fixture_set_id],
        )?;
        Ok(deleted > 0)
    }
}
