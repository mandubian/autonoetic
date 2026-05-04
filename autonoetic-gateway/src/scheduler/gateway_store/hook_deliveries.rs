use anyhow::Result;
use rusqlite::params;

use super::GatewayStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDeliveryRecord {
    pub event_id: String,
    pub hook_event: String,
    pub hook_action: String,
    pub status: String,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl GatewayStore {
    pub fn upsert_hook_delivery(
        &self,
        event_id: &str,
        hook_event: &str,
        hook_action: &str,
        status: &str,
        attempt_count: i64,
        last_error: Option<&str>,
        now: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO hook_deliveries (
                event_id, hook_event, hook_action, status, attempt_count, last_error, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
            ON CONFLICT(event_id, hook_event, hook_action) DO UPDATE SET
                status = excluded.status,
                attempt_count = excluded.attempt_count,
                last_error = excluded.last_error,
                updated_at = excluded.updated_at",
            params![
                event_id,
                hook_event,
                hook_action,
                status,
                attempt_count,
                last_error,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn get_hook_delivery(
        &self,
        event_id: &str,
        hook_event: &str,
        hook_action: &str,
    ) -> Result<Option<HookDeliveryRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT event_id, hook_event, hook_action, status, attempt_count, last_error, created_at, updated_at
             FROM hook_deliveries
             WHERE event_id = ?1 AND hook_event = ?2 AND hook_action = ?3
             LIMIT 1",
        )?;
        let result = stmt.query_row(params![event_id, hook_event, hook_action], |row| {
            Ok(HookDeliveryRecord {
                event_id: row.get(0)?,
                hook_event: row.get(1)?,
                hook_action: row.get(2)?,
                status: row.get(3)?,
                attempt_count: row.get(4)?,
                last_error: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        });

        match result {
            Ok(record) => Ok(Some(record)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
