use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessageRecord {
    pub message_id: String,
    pub sender_session_id: String,
    pub sender_agent_id: String,
    pub target_pattern: String,
    pub message: String,
    pub created_at: String,
}

pub(super) fn save_agent_message(conn: &Connection, record: &AgentMessageRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_messages (message_id, sender_session_id, sender_agent_id, target_pattern, message, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.message_id,
            record.sender_session_id,
            record.sender_agent_id,
            record.target_pattern,
            record.message,
            record.created_at
        ],
    )?;
    Ok(())
}

pub(super) fn insert_message_delivery(
    conn: &Connection,
    message_id: &str,
    target_session_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO agent_message_deliveries (message_id, target_session_id) VALUES (?1, ?2)",
        params![message_id, target_session_id],
    )?;
    Ok(())
}

pub(super) fn fetch_undelivered_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<AgentMessageRecord>> {
    let mut stmt = conn.prepare(
        "SELECT m.message_id, m.sender_session_id, m.sender_agent_id, m.target_pattern, m.message, m.created_at
         FROM agent_messages m
         JOIN agent_message_deliveries d ON m.message_id = d.message_id
         WHERE d.target_session_id = ?1 AND d.delivered_at IS NULL
         ORDER BY m.created_at ASC"
    )?;
    let rows = stmt.query_map(params![session_id], |row| {
        Ok(AgentMessageRecord {
            message_id: row.get(0)?,
            sender_session_id: row.get(1)?,
            sender_agent_id: row.get(2)?,
            target_pattern: row.get(3)?,
            message: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

pub(super) fn mark_message_delivered(
    conn: &Connection,
    message_id: &str,
    target_session_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_message_deliveries SET delivered_at = ?1 WHERE message_id = ?2 AND target_session_id = ?3",
        params![chrono::Utc::now().to_rfc3339(), message_id, target_session_id],
    )?;
    Ok(())
}
