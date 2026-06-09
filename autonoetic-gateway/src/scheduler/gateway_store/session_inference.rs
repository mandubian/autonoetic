use super::GatewayStore;
use crate::runtime::inference_profile::SessionInferenceBinding;
use anyhow::Result;
use rusqlite::params;

impl GatewayStore {
    pub fn get_session_inference_binding(
        &self,
        root_session_id: &str,
    ) -> Result<Option<SessionInferenceBinding>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT root_session_id, preset_override, reason, set_by, set_at
             FROM session_inference_bindings WHERE root_session_id = ?1",
        )?;
        let mut rows = stmt.query(params![root_session_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(SessionInferenceBinding {
                root_session_id: row.get(0)?,
                preset_override: row.get(1)?,
                reason: row.get(2)?,
                set_by: row.get(3)?,
                set_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn upsert_session_inference_binding(
        &self,
        root_session_id: &str,
        preset_override: Option<&str>,
        reason: Option<&str>,
        set_by: &str,
    ) -> Result<SessionInferenceBinding> {
        let set_at = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO session_inference_bindings (root_session_id, preset_override, reason, set_by, set_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(root_session_id) DO UPDATE SET
               preset_override = excluded.preset_override,
               reason = excluded.reason,
               set_by = excluded.set_by,
               set_at = excluded.set_at",
            params![root_session_id, preset_override, reason, set_by, set_at],
        )?;
        Ok(SessionInferenceBinding {
            root_session_id: root_session_id.to_string(),
            preset_override: preset_override.map(str::to_string),
            reason: reason.map(str::to_string),
            set_by: set_by.to_string(),
            set_at,
        })
    }

    pub fn delete_session_inference_binding(&self, root_session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM session_inference_bindings WHERE root_session_id = ?1",
            params![root_session_id],
        )?;
        Ok(n > 0)
    }
}
