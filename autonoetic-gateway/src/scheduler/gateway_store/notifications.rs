use anyhow::Result;
use autonoetic_types::notification::{NotificationRecord, NotificationStatus, NotificationType};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    pub fn create_notification_record(&self, n: &NotificationRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let payload = serde_json::to_string(&n.payload)?;
        conn.execute(
            "INSERT INTO notifications (
                notification_id, notification_type, request_id, target_session_id, target_agent_id,
                workflow_id, task_id, payload, status, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                n.notification_id,
                serde_json::to_string(&n.notification_type)?,
                n.request_id,
                n.target_session_id,
                n.target_agent_id,
                n.workflow_id,
                n.task_id,
                payload,
                serde_json::to_string(&n.status)?,
                n.created_at
            ],
        )?;
        Ok(())
    }

    pub fn create_notification(&self, session_id: &str, payload: &serde_json::Value) -> Result<()> {
        let n = NotificationRecord::new(
            format!("ntf-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            NotificationType::ApprovalResolved,
            session_id.to_string(),
            payload.clone(),
        );
        self.create_notification_record(&n)
    }

    pub fn list_pending_notifications(&self) -> Result<Vec<NotificationRecord>> {
        let conn = self.conn.lock().unwrap();
        let status = serde_json::to_string(&NotificationStatus::Pending)?;
        let mut stmt = conn.prepare(
            "SELECT notification_id FROM notifications WHERE status = ?1 ORDER BY created_at ASC, notification_id ASC",
        )?;
        let rows = stmt.query_map(params![status], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            let id = id_result?;
            if let Some(n) = Self::get_notification_with_conn(&conn, &id)? {
                results.push(n);
            }
        }
        Ok(results)
    }

    fn get_notification_with_conn(
        conn: &Connection,
        id: &str,
    ) -> Result<Option<NotificationRecord>> {
        conn.query_row(
            "SELECT * FROM notifications WHERE notification_id = ?1",
            params![id],
            |row| {
                let n_type_str: String = row.get(1)?;
                let status_str: String = row.get(8)?;
                let payload_str: String = row.get(7)?;

                let notification_type = serde_json::from_str(&n_type_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let payload = serde_json::from_str(&payload_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        7,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                let status = serde_json::from_str(&status_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        8,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

                Ok(NotificationRecord {
                    notification_id: row.get(0)?,
                    notification_type,
                    request_id: row.get(2)?,
                    target_session_id: row.get(3)?,
                    target_agent_id: row.get(4)?,
                    workflow_id: row.get(5)?,
                    task_id: row.get(6)?,
                    payload,
                    status,
                    created_at: row.get(9)?,
                    action_completed_at: row.get(10)?,
                    delivered_at: row.get(11)?,
                    consumed_at: row.get(12)?,
                    attempt_count: row.get(13)?,
                    last_attempt_at: row.get(14)?,
                    error_message: row.get(15)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_notification(&self, id: &str) -> Result<Option<NotificationRecord>> {
        let conn = self.conn.lock().unwrap();
        Self::get_notification_with_conn(&conn, id)
    }

    pub fn update_notification_status(&self, id: &str, status: NotificationStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let status_str = serde_json::to_string(&status)?;
        let now = chrono::Utc::now().to_rfc3339();

        match status {
            NotificationStatus::ActionExecuted => {
                conn.execute(
                    "UPDATE notifications SET status = ?1, action_completed_at = ?2 WHERE notification_id = ?3",
                    params![status_str, now, id],
                )?;
            }
            NotificationStatus::Delivered => {
                conn.execute(
                    "UPDATE notifications SET status = ?1, delivered_at = ?2 WHERE notification_id = ?3",
                    params![status_str, now, id],
                )?;
            }
            NotificationStatus::Consumed => {
                conn.execute(
                    "UPDATE notifications SET status = ?1, consumed_at = ?2 WHERE notification_id = ?3",
                    params![status_str, now, id],
                )?;
            }
            _ => {
                conn.execute(
                    "UPDATE notifications SET status = ?1 WHERE notification_id = ?2",
                    params![status_str, id],
                )?;
            }
        }
        Ok(())
    }

    pub fn mark_consumed(&self, id: &str) -> Result<()> {
        self.update_notification_status(id, NotificationStatus::Consumed)
    }

    pub fn list_notifications_for_session(
        &self,
        session_id: &str,
        status: NotificationStatus,
    ) -> Result<Vec<NotificationRecord>> {
        let conn = self.conn.lock().unwrap();
        let status_str = serde_json::to_string(&status)?;
        let mut stmt = conn.prepare("SELECT notification_id FROM notifications WHERE target_session_id = ?1 AND status = ?2")?;
        let rows = stmt.query_map(params![session_id, status_str], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for id_result in rows {
            if let Ok(id) = id_result {
                if let Ok(Some(n)) = Self::get_notification_with_conn(&conn, &id) {
                    results.push(n);
                }
            }
        }
        Ok(results)
    }

    pub fn increment_attempt(&self, id: &str, error: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE notifications SET attempt_count = attempt_count + 1, last_attempt_at = ?1, error_message = ?2 WHERE notification_id = ?3",
            params![now, error, id],
        )?;
        Ok(())
    }

    /// Delete consumed notifications older than `max_age_hours` and failed
    /// notifications created before the cutoff.
    pub fn cleanup_stale_notifications(&self, max_age_hours: u64) -> Result<u64> {
        use rusqlite::params;
        let conn = self.conn.lock().unwrap();
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::hours(max_age_hours as i64)).to_rfc3339();
        let rows = conn.execute(
            "DELETE FROM notifications WHERE consumed_at < ?1 OR (status IN (?2, ?3) AND created_at < ?4)",
            params![
                cutoff,
                serde_json::to_string(&NotificationStatus::Failed)?,
                serde_json::to_string(&NotificationStatus::Suppressed)?,
                cutoff
            ],
        )?;
        Ok(rows as u64)
    }
}
