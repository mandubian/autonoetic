use anyhow::Result;
use autonoetic_types::background::{
    UserInteraction, UserInteractionAnswer, UserInteractionKind, UserInteractionOption,
    UserInteractionStatus,
};
use rusqlite::{params, Connection, OptionalExtension};

use super::GatewayStore;

impl GatewayStore {
    pub fn create_user_interaction(&self, interaction: &UserInteraction) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let options_json = if interaction.options.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&interaction.options)?)
        };
        conn.execute(
            "INSERT INTO user_interactions (
                interaction_id, session_id, root_session_id, workflow_id, task_id,
                agent_id, turn_id, kind, question, context, options_json, allow_freeform,
                status, created_at, expires_at, checkpoint_turn_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                interaction.interaction_id,
                interaction.session_id,
                interaction.root_session_id,
                interaction.workflow_id,
                interaction.task_id,
                interaction.agent_id,
                interaction.turn_id,
                interaction.kind.as_str(),
                interaction.question,
                interaction.context,
                options_json,
                if interaction.allow_freeform {
                    1i32
                } else {
                    0i32
                },
                "pending",
                interaction.created_at,
                interaction.expires_at,
                interaction.checkpoint_turn_id,
            ],
        )?;
        Ok(())
    }

    pub fn get_user_interaction(&self, interaction_id: &str) -> Result<Option<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        Self::get_user_interaction_with_conn(&conn, interaction_id)
    }

    fn get_user_interaction_with_conn(
        conn: &Connection,
        interaction_id: &str,
    ) -> Result<Option<UserInteraction>> {
        conn.query_row(
            "SELECT interaction_id, session_id, root_session_id, workflow_id, task_id,
                    agent_id, turn_id, kind, question, context, options_json, allow_freeform,
                    status, answer_option_id, answer_text, answered_by, created_at, answered_at,
                    expires_at, checkpoint_turn_id
             FROM user_interactions WHERE interaction_id = ?1",
            params![interaction_id],
            |row| {
                let kind_str: String = row.get(7)?;
                let status_str: String = row.get(12)?;
                let options_json_str: Option<String> = row.get(10)?;

                let kind = match kind_str.as_str() {
                    "clarification" => UserInteractionKind::Clarification,
                    "decision" => UserInteractionKind::Decision,
                    "proposal" => UserInteractionKind::Proposal,
                    "confirmation" => UserInteractionKind::Confirmation,
                    _ => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            format!("invalid user_interactions.kind: {}", kind_str).into(),
                        ))
                    }
                };
                let status = match status_str.as_str() {
                    "pending" => UserInteractionStatus::Pending,
                    "answered" => UserInteractionStatus::Answered,
                    "cancelled" => UserInteractionStatus::Cancelled,
                    "expired" => UserInteractionStatus::Expired,
                    _ => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            12,
                            rusqlite::types::Type::Text,
                            format!("invalid user_interactions.status: {}", status_str).into(),
                        ))
                    }
                };
                let options: Vec<UserInteractionOption> = match options_json_str {
                    Some(s) => serde_json::from_str(&s).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            10,
                            rusqlite::types::Type::Text,
                            e.into(),
                        )
                    })?,
                    None => Vec::new(),
                };

                Ok(UserInteraction {
                    interaction_id: row.get(0)?,
                    session_id: row.get(1)?,
                    root_session_id: row.get(2)?,
                    workflow_id: row.get(3)?,
                    task_id: row.get(4)?,
                    agent_id: row.get(5)?,
                    turn_id: row.get(6)?,
                    kind,
                    question: row.get(8)?,
                    context: row.get(9)?,
                    options,
                    allow_freeform: row.get::<_, i32>(11)? != 0,
                    status,
                    answer_option_id: row.get(13)?,
                    answer_text: row.get(14)?,
                    answered_by: row.get(15)?,
                    created_at: row.get(16)?,
                    answered_at: row.get(17)?,
                    expires_at: row.get(18)?,
                    checkpoint_turn_id: row.get(19)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn answer_user_interaction(&self, answer: &UserInteractionAnswer) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        anyhow::ensure!(
            !answer.interaction_id.trim().is_empty(),
            "interaction_id must not be empty"
        );
        let interaction = Self::get_user_interaction_with_conn(&conn, &answer.interaction_id)?
            .ok_or_else(|| {
                anyhow::anyhow!("User interaction '{}' not found", answer.interaction_id)
            })?;
        anyhow::ensure!(
            interaction.status == UserInteractionStatus::Pending,
            "User interaction '{}' is {:?}; only pending interactions can be answered",
            answer.interaction_id,
            interaction.status
        );

        let answer_option_id = answer
            .answer_option_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let answer_text = answer
            .answer_text
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned();
        anyhow::ensure!(
            answer_option_id.is_some() || answer_text.is_some(),
            "Must provide either answer_option_id or non-empty answer_text"
        );
        anyhow::ensure!(
            !(answer_option_id.is_some() && answer_text.is_some()),
            "Provide exactly one of answer_option_id or answer_text"
        );

        if let Some(ref oid) = answer_option_id {
            let valid = interaction.options.iter().any(|opt| opt.id == *oid);
            anyhow::ensure!(
                valid,
                "Invalid answer_option_id '{}' for interaction '{}'",
                oid,
                answer.interaction_id
            );
        }
        if answer_text.is_some() {
            anyhow::ensure!(
                interaction.allow_freeform,
                "Interaction '{}' does not allow freeform answers",
                answer.interaction_id
            );
        }

        let now = chrono::Utc::now().to_rfc3339();
        let changed = conn.execute(
            "UPDATE user_interactions SET
                status = 'answered', answer_option_id = ?1, answer_text = ?2,
                answered_by = ?3, answered_at = ?4
             WHERE interaction_id = ?5 AND status = 'pending'",
            params![
                answer_option_id,
                answer_text,
                answer.answered_by,
                now,
                answer.interaction_id,
            ],
        )?;
        anyhow::ensure!(
            changed == 1,
            "User interaction '{}' was not updated (status changed concurrently)",
            answer.interaction_id
        );
        Ok(())
    }

    pub fn cancel_user_interaction(&self, interaction_id: &str, reason: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        anyhow::ensure!(
            !interaction_id.trim().is_empty(),
            "interaction_id must not be empty"
        );
        anyhow::ensure!(!reason.trim().is_empty(), "reason must not be empty");

        let interaction = Self::get_user_interaction_with_conn(&conn, interaction_id)?
            .ok_or_else(|| anyhow::anyhow!("User interaction '{}' not found", interaction_id))?;
        anyhow::ensure!(
            interaction.status == UserInteractionStatus::Pending,
            "User interaction '{}' is {:?}; only pending interactions can be cancelled",
            interaction_id,
            interaction.status
        );

        let changed = conn.execute(
            "UPDATE user_interactions SET status = 'cancelled', answer_text = ?1 WHERE interaction_id = ?2 AND status = 'pending'",
            params![reason, interaction_id],
        )?;
        anyhow::ensure!(
            changed == 1,
            "User interaction '{}' was not cancelled (status changed concurrently)",
            interaction_id
        );
        Ok(())
    }

    pub fn expire_timed_out_interactions(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions
             WHERE status = 'pending' AND expires_at IS NOT NULL AND expires_at < ?1",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut expired_ids = Vec::new();
        for row in rows {
            let id = row?;
            conn.execute(
                "UPDATE user_interactions SET status = 'expired' WHERE interaction_id = ?1",
                params![id],
            )?;
            expired_ids.push(id);
        }
        Ok(expired_ids)
    }

    pub fn get_pending_interactions_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions WHERE session_id = ?1 AND status = 'pending'",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            let id = row?;
            if let Some(interaction) = Self::get_user_interaction_with_conn(&conn, &id)? {
                results.push(interaction);
            }
        }
        Ok(results)
    }

    pub fn get_pending_interactions_for_root_session(
        &self,
        root_session_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions WHERE root_session_id = ?1 AND status = 'pending'",
        )?;
        let rows = stmt.query_map(params![root_session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            let id = row?;
            if let Some(interaction) = Self::get_user_interaction_with_conn(&conn, &id)? {
                results.push(interaction);
            }
        }
        Ok(results)
    }

    pub fn get_answered_standalone_interactions(&self) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions \
             WHERE status = 'answered' AND (workflow_id IS NULL OR workflow_id = '') \
             ORDER BY answered_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            let id = row?;
            if let Some(interaction) = Self::get_user_interaction_with_conn(&conn, &id)? {
                results.push(interaction);
            }
        }
        Ok(results)
    }

    pub fn list_user_interactions_for_session_trace(
        &self,
        session_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions \
             WHERE session_id = ?1 OR root_session_id = ?1 \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            let id = row?;
            if let Some(interaction) = Self::get_user_interaction_with_conn(&conn, &id)? {
                results.push(interaction);
            }
        }
        Ok(results)
    }

    pub fn list_user_interactions_for_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<UserInteraction>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT interaction_id FROM user_interactions \
             WHERE workflow_id = ?1 \
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![workflow_id], |row| {
            let id: String = row.get(0)?;
            Ok(id)
        })?;

        let mut results = Vec::new();
        for row in rows {
            let id = row?;
            if let Some(interaction) = Self::get_user_interaction_with_conn(&conn, &id)? {
                results.push(interaction);
            }
        }
        Ok(results)
    }
}
