use anyhow::Result;
use rusqlite::{params};

use crate::runtime::human_gate::GateMessage;

use super::GatewayStore;

impl GatewayStore {
    pub fn add_gate_message(
        &self,
        gate_id: &str,
        sender: &str,
        content: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO gate_messages (gate_id, sender, content, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                gate_id,
                sender,
                content,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_gate_messages(&self, gate_id: &str) -> Result<Vec<GateMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, gate_id, sender, content, created_at
             FROM gate_messages
             WHERE gate_id = ?1
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![gate_id], |row| {
            Ok(GateMessage {
                id: row.get(0)?,
                gate_id: row.get(1)?,
                sender: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        let mut messages = Vec::new();
        for msg in rows {
            messages.push(msg?);
        }
        Ok(messages)
    }
}
