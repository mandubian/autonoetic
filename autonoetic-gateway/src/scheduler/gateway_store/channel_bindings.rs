//! Store access for `operator_channel_bindings` (#393, P3.c).
//!
//! Maps an external channel conversation `(channel, external_id)` to a room
//! (`root_session_id`) so a Discord thread / WhatsApp chat survives reconnects
//! and routes replies back as `Operator`-seat events. Channels reach these over
//! RPC (`channel.bind` / `channel.resolve`) — they are API clients, never direct
//! store readers (Separation of Powers, #390).

use anyhow::Result;
use autonoetic_types::channel::ChannelBinding;
use rusqlite::OptionalExtension;

use super::GatewayStore;

impl GatewayStore {
    /// Upsert a binding (idempotent on the `(channel, external_id)` natural key).
    /// Rebinding the same conversation to a new room updates `root_session_id`
    /// and `updated_at` while preserving the original `created_at`. Returns the
    /// resulting binding.
    pub fn bind_channel(
        &self,
        channel: &str,
        external_id: &str,
        root_session_id: &str,
    ) -> Result<ChannelBinding> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO operator_channel_bindings
                (channel, external_id, root_session_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(channel, external_id) DO UPDATE SET
                root_session_id = excluded.root_session_id,
                updated_at = excluded.updated_at",
            rusqlite::params![channel, external_id, root_session_id, now],
        )?;
        // Read back so the caller sees the persisted row (notably the original
        // created_at on a rebind).
        conn.query_row(
            "SELECT channel, external_id, root_session_id, created_at, updated_at
             FROM operator_channel_bindings WHERE channel = ?1 AND external_id = ?2",
            rusqlite::params![channel, external_id],
            |row| {
                Ok(ChannelBinding {
                    channel: row.get(0)?,
                    external_id: row.get(1)?,
                    root_session_id: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .map_err(Into::into)
    }

    /// Look up the binding for a conversation, or `None` if it is unbound.
    pub fn resolve_channel_binding(
        &self,
        channel: &str,
        external_id: &str,
    ) -> Result<Option<ChannelBinding>> {
        let conn = self.conn.lock().unwrap();
        let binding = conn
            .query_row(
                "SELECT channel, external_id, root_session_id, created_at, updated_at
                 FROM operator_channel_bindings WHERE channel = ?1 AND external_id = ?2",
                rusqlite::params![channel, external_id],
                |row| {
                    Ok(ChannelBinding {
                        channel: row.get(0)?,
                        external_id: row.get(1)?,
                        root_session_id: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(binding)
    }
}
