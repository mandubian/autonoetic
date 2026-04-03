use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use rusqlite::{params, Connection, OptionalExtension};

impl GatewayStore {
    // --- Artifact refs ---

    pub fn create_artifact_ref(&self, record: &ArtifactRefRecord) -> Result<()> {
        if record.ref_id.is_empty() {
            return Err(anyhow::anyhow!("artifact ref_id must not be empty"));
        }
        if record.scope_id.is_empty() {
            return Err(anyhow::anyhow!("artifact scope_id must not be empty"));
        }
        if record.artifact_id.is_empty() {
            return Err(anyhow::anyhow!("artifact_id must not be empty"));
        }
        if record.artifact_digest.is_empty() {
            return Err(anyhow::anyhow!("artifact_digest must not be empty"));
        }
        if record.created_by_agent_id.is_empty() {
            return Err(anyhow::anyhow!("created_by_agent_id must not be empty"));
        }

        Self::parse_rfc3339_utc(&record.created_at, "created_at")?;
        if let Some(expires_at) = record.expires_at.as_deref() {
            Self::parse_rfc3339_utc(expires_at, "expires_at")?;
        }
        if let Some(revoked_at) = record.revoked_at.as_deref() {
            Self::parse_rfc3339_utc(revoked_at, "revoked_at")?;
        }

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO artifact_refs (
                ref_id, scope_type, scope_id, artifact_id, artifact_digest, created_by_agent_id,
                created_at, expires_at, revoked_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.ref_id,
                record.scope_type.as_str(),
                record.scope_id,
                record.artifact_id,
                record.artifact_digest,
                record.created_by_agent_id,
                record.created_at,
                record.expires_at,
                record.revoked_at
            ],
        )?;
        Ok(())
    }

    pub fn resolve_artifact_ref(
        &self,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
        ref_id: &str,
    ) -> Result<Option<ArtifactRefRecord>> {
        let conn = self.conn.lock().unwrap();
        Self::resolve_artifact_ref_with_conn(&conn, scope_type, scope_id, ref_id)
    }

    pub fn list_artifact_refs_for_scope(
        &self,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
    ) -> Result<Vec<ArtifactRefRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT
                ref_id, scope_type, scope_id, artifact_id, artifact_digest, created_by_agent_id,
                created_at, expires_at, revoked_at
             FROM artifact_refs
             WHERE scope_type = ?1 AND scope_id = ?2
             ORDER BY created_at ASC, ref_id ASC",
        )?;
        let rows = stmt.query_map(
            params![scope_type.as_str(), scope_id],
            Self::artifact_ref_from_row,
        )?;

        let now = chrono::Utc::now();
        let mut refs = Vec::new();
        for row in rows {
            let record = row?;
            if Self::artifact_ref_is_active(&record, now)? {
                refs.push(record);
            }
        }
        Ok(refs)
    }

    pub fn revoke_artifact_ref(
        &self,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
        ref_id: &str,
        revoked_at: Option<&str>,
    ) -> Result<bool> {
        let revoked_at = revoked_at
            .map(str::to_string)
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        Self::parse_rfc3339_utc(&revoked_at, "revoked_at")?;

        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE artifact_refs
             SET revoked_at = ?1
             WHERE scope_type = ?2
               AND scope_id = ?3
               AND ref_id = ?4
               AND revoked_at IS NULL",
            params![revoked_at, scope_type.as_str(), scope_id, ref_id],
        )?;
        Ok(updated > 0)
    }

    fn resolve_artifact_ref_with_conn(
        conn: &Connection,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
        ref_id: &str,
    ) -> Result<Option<ArtifactRefRecord>> {
        let record = conn
            .query_row(
                "SELECT
                    ref_id, scope_type, scope_id, artifact_id, artifact_digest, created_by_agent_id,
                    created_at, expires_at, revoked_at
                 FROM artifact_refs
                 WHERE scope_type = ?1 AND scope_id = ?2 AND ref_id = ?3",
                params![scope_type.as_str(), scope_id, ref_id],
                Self::artifact_ref_from_row,
            )
            .optional()?;

        let Some(record) = record else {
            return Ok(None);
        };

        if Self::artifact_ref_is_active(&record, chrono::Utc::now())? {
            Ok(Some(record))
        } else {
            Ok(None)
        }
    }

    fn artifact_ref_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRefRecord> {
        let scope_type_raw: String = row.get(1)?;
        let scope_type = ArtifactRefScopeType::from_str(&scope_type_raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid artifact ref scope_type: {scope_type_raw}"),
                )),
            )
        })?;

        Ok(ArtifactRefRecord {
            ref_id: row.get(0)?,
            scope_type,
            scope_id: row.get(2)?,
            artifact_id: row.get(3)?,
            artifact_digest: row.get(4)?,
            created_by_agent_id: row.get(5)?,
            created_at: row.get(6)?,
            expires_at: row.get(7)?,
            revoked_at: row.get(8)?,
        })
    }

    fn artifact_ref_is_active(
        record: &ArtifactRefRecord,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        if let Some(revoked_at) = record.revoked_at.as_deref() {
            Self::parse_rfc3339_utc(revoked_at, "revoked_at")?;
            return Ok(false);
        }
        if let Some(expires_at) = record.expires_at.as_deref() {
            let expires_at = Self::parse_rfc3339_utc(expires_at, "expires_at")?;
            if now >= expires_at {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn parse_rfc3339_utc(
        value: &str,
        field_name: &'static str,
    ) -> Result<chrono::DateTime<chrono::Utc>> {
        let dt = chrono::DateTime::parse_from_rfc3339(value).map_err(|e| {
            anyhow::anyhow!(
                "invalid RFC3339 timestamp for artifact_refs.{}: {}",
                field_name,
                e
            )
        })?;
        Ok(dt.with_timezone(&chrono::Utc))
    }
}
