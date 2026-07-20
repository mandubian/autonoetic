use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::artifact::{ArtifactRefRecord, ArtifactRefScopeType};
use rusqlite::{params, Connection, OptionalExtension};

impl GatewayStore {
    // --- Artifact refs ---
    //
    // Fork lineage (`record_fork_lineage`, `get_fork_source`,
    // `backfill_fork_lineage_from_causal_events`, `fork_ancestor_roots`) moved
    // to `fork_lineage.rs` (#814). `fork_ancestor_roots` stays `pub(super)` so
    // it can still be used below for artifact-ref resolution across fork
    // boundaries.

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
        if record.artifact_manifest_digest.is_empty() {
            return Err(anyhow::anyhow!(
                "artifact_manifest_digest must not be empty"
            ));
        }
        if record.artifact_canonical_digest.is_empty() {
            return Err(anyhow::anyhow!(
                "artifact_canonical_digest must not be empty"
            ));
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
                ref_id, scope_type, scope_id, artifact_id, artifact_digest, artifact_canonical_digest,
                created_by_agent_id, created_at, expires_at, revoked_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.ref_id,
                record.scope_type.as_str(),
                record.scope_id,
                record.artifact_id,
                record.artifact_manifest_digest,
                record.artifact_canonical_digest,
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

    /// Resolves an artifact ref by ref_id across all scopes accessible from the given session.
    ///
    /// Lookup priority: global → workflow (if any for this root session) → session
    /// → root session → fork ancestor roots (parent, grandparent, …).
    /// Pass the current session_id; root session and workflow lookup are derived automatically.
    pub fn resolve_artifact_ref_any_scope(
        &self,
        ref_id: &str,
        session_id: &str,
    ) -> Result<Option<ArtifactRefRecord>> {
        let conn = self.conn.lock().unwrap();

        // 1. Try global scope first
        if let Some(r) = Self::resolve_artifact_ref_with_conn(
            &conn,
            ArtifactRefScopeType::Global,
            "__global__",
            ref_id,
        )? {
            return Ok(Some(r));
        }

        // 2. Try workflow scope for this root session only
        let root_sid = crate::runtime::content_store::root_session_id(session_id);
        let workflow_candidate: Option<String> = conn
            .query_row(
                "SELECT workflow_id FROM workflow_index WHERE root_session_id = ?1",
                params![root_sid],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(wf_id) = workflow_candidate {
            if let Some(r) = Self::resolve_artifact_ref_with_conn(
                &conn,
                ArtifactRefScopeType::Workflow,
                &wf_id,
                ref_id,
            )? {
                return Ok(Some(r));
            }
        }

        // 3. Try exact session scope
        if let Some(r) = Self::resolve_artifact_ref_with_conn(
            &conn,
            ArtifactRefScopeType::Session,
            session_id,
            ref_id,
        )? {
            return Ok(Some(r));
        }

        // 4. Try root session scope
        if root_sid != session_id {
            if let Some(r) = Self::resolve_artifact_ref_with_conn(
                &conn,
                ArtifactRefScopeType::Session,
                root_sid,
                ref_id,
            )? {
                return Ok(Some(r));
            }
        }

        // 5. Walk fork ancestor roots — a forked session inherits its parent's
        //    artifact refs. For each ancestor, check both its workflow scope
        //    (if linked to one) and its session scope, from nearest to furthest.
        for ancestor_root in self.fork_ancestor_roots(&conn, session_id) {
            if ancestor_root == root_sid {
                continue; // already checked in steps 2 and 4
            }

            // 5a. Workflow scope for this ancestor's root session.
            let ancestor_wf: Option<String> = conn
                .query_row(
                    "SELECT workflow_id FROM workflow_index WHERE root_session_id = ?1",
                    params![&ancestor_root],
                    |row| row.get(0),
                )
                .optional()
                .ok()
                .flatten();
            if let Some(wf_id) = ancestor_wf {
                if let Some(r) = Self::resolve_artifact_ref_with_conn(
                    &conn,
                    ArtifactRefScopeType::Workflow,
                    &wf_id,
                    ref_id,
                )? {
                    return Ok(Some(r));
                }
            }

            // 5b. Session scope for this ancestor's root.
            if let Some(r) = Self::resolve_artifact_ref_with_conn(
                &conn,
                ArtifactRefScopeType::Session,
                &ancestor_root,
                ref_id,
            )? {
                return Ok(Some(r));
            }
        }

        Ok(None)
    }

    /// Find an active short ref (`ar.*`) for a canonical artifact ID (`art_*`)
    /// that is resolvable from the given session. Searches scopes in priority
    /// order: Global → Workflow → Session → Root → Fork ancestor roots.
    ///
    /// Returns `None` if no active ref exists, or if all existing refs are
    /// expired, revoked, or scoped outside the caller's reach.
    pub fn find_active_ref_for_artifact(
        &self,
        artifact_id: &str,
        session_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let now = chrono::Utc::now();

        // Collect all non-revoked refs for this artifact
        let mut stmt = conn.prepare(
            "SELECT ref_id, scope_type, scope_id, expires_at
             FROM artifact_refs
             WHERE artifact_id = ?1 AND revoked_at IS NULL
             ORDER BY created_at DESC",
        )?;

        let candidates: Vec<(String, String, String, Option<String>)> = stmt
            .query_map(params![artifact_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .filter(|(_, _, _, expires_at)| {
                if let Some(exp) = expires_at {
                    Self::parse_rfc3339_utc(exp, "expires_at")
                        .map(|exp_dt| exp_dt > now)
                        .unwrap_or(false)
                } else {
                    true
                }
            })
            .collect();

        let root_sid = crate::runtime::content_store::root_session_id(session_id);
        let wf_candidate: Option<String> = conn
            .query_row(
                "SELECT workflow_id FROM workflow_index WHERE root_session_id = ?1",
                params![root_sid],
                |row| row.get(0),
            )
            .optional()?;

        // Build a map of fork ancestor root → rank (4, 5, 6, … nearest first).
        let fork_ancestors = self.fork_ancestor_roots(&conn, session_id);

        // Also resolve each ancestor's workflow so workflow-scoped refs resolve
        // across fork boundaries (artifact refs are often workflow-scoped).
        let ancestor_wfs: Vec<(String, u8)> = fork_ancestors
            .iter()
            .enumerate()
            .filter_map(|(i, ancestor_root)| {
                let wf: Option<String> = conn
                    .query_row(
                        "SELECT workflow_id FROM workflow_index WHERE root_session_id = ?1",
                        params![ancestor_root],
                        |row| row.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                wf.map(|w| (w, 4 + i as u8))
            })
            .collect();

        let fork_rank = |scope_id: &str| -> Option<u8> {
            fork_ancestors
                .iter()
                .position(|a| a == scope_id)
                .map(|i| 4 + i as u8)
        };

        let rank = |scope_type: &str, scope_id: &str| -> u8 {
            if scope_type == "global" { return 0; }
            if scope_type == "workflow" {
                if let Some(ref wf) = wf_candidate {
                    if scope_id == wf.as_str() { return 1; }
                }
                // Check ancestor workflows.
                if let Some((_, r)) = ancestor_wfs.iter().find(|(w, _)| w == scope_id) {
                    return *r;
                }
                return 255;
            }
            if scope_type == "session" {
                if scope_id == session_id { return 2; }
                if scope_id == root_sid { return 3; }
                if let Some(r) = fork_rank(scope_id) { return r; }
                return 255;
            }
            255
        };

        let best = candidates
            .iter()
            .map(|(ref_id, st, sid, _)| (ref_id, rank(st, sid)))
            .filter(|(_, r)| *r < 255)
            .min_by_key(|(_, r)| *r);

        Ok(best.map(|(ref_id, _)| ref_id.clone()))
    }

    pub fn list_artifact_refs_for_scope(
        &self,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
    ) -> Result<Vec<ArtifactRefRecord>> {
        let conn = self.conn.lock().unwrap();
        Self::list_refs_for_scope_with_conn(&conn, scope_type, scope_id)
    }

    fn list_refs_for_scope_with_conn(
        conn: &Connection,
        scope_type: ArtifactRefScopeType,
        scope_id: &str,
    ) -> Result<Vec<ArtifactRefRecord>> {
        let mut stmt = conn.prepare(
            "SELECT
                ref_id, scope_type, scope_id, artifact_id, artifact_digest, artifact_canonical_digest,
                created_by_agent_id, created_at, expires_at, revoked_at
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

    /// List all active artifact refs visible from the given session, for
    /// repair hints when an agent guesses a ref that doesn't exist
    /// (anti-thrash, #312). Visibility mirrors `resolve_artifact_ref_any_scope`:
    /// Global → workflow of the root session → exact session → root session →
    /// fork ancestor roots (each with their workflow scope).
    pub fn list_artifact_refs_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ArtifactRefRecord>> {
        let conn = self.conn.lock().unwrap();
        let root_sid = crate::runtime::content_store::root_session_id(session_id);

        let wf_for = |conn: &Connection, root: &str| -> Result<Option<String>> {
            Ok(conn
                .query_row(
                    "SELECT workflow_id FROM workflow_index WHERE root_session_id = ?1",
                    params![root],
                    |row| row.get(0),
                )
                .optional()?)
        };

        let mut scopes: Vec<(ArtifactRefScopeType, String)> =
            vec![(ArtifactRefScopeType::Global, "__global__".to_string())];
        if let Some(wf) = wf_for(&conn, root_sid)? {
            scopes.push((ArtifactRefScopeType::Workflow, wf));
        }
        scopes.push((ArtifactRefScopeType::Session, session_id.to_string()));
        if root_sid != session_id {
            scopes.push((ArtifactRefScopeType::Session, root_sid.to_string()));
        }
        for ancestor_root in self.fork_ancestor_roots(&conn, session_id) {
            if ancestor_root == root_sid {
                continue;
            }
            if let Some(wf) = wf_for(&conn, &ancestor_root)? {
                scopes.push((ArtifactRefScopeType::Workflow, wf));
            }
            scopes.push((ArtifactRefScopeType::Session, ancestor_root));
        }

        let mut out: Vec<ArtifactRefRecord> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (scope_type, scope_id) in scopes {
            for record in Self::list_refs_for_scope_with_conn(&conn, scope_type, &scope_id)? {
                if seen.insert(record.ref_id.clone()) {
                    out.push(record);
                }
            }
        }
        Ok(out)
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
                    ref_id, scope_type, scope_id, artifact_id, artifact_digest, artifact_canonical_digest,
                    created_by_agent_id, created_at, expires_at, revoked_at
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
            artifact_manifest_digest: row.get(4)?,
            artifact_canonical_digest: row.get(5)?,
            created_by_agent_id: row.get(6)?,
            created_at: row.get(7)?,
            expires_at: row.get(8)?,
            revoked_at: row.get(9)?,
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

    pub fn promote_artifact_ref_to_global(&self, artifact_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE artifact_refs
             SET scope_type = 'global', scope_id = '__global__'
             WHERE artifact_id = ?1
               AND revoked_at IS NULL
               AND scope_type != 'global'",
            params![artifact_id],
        )?;
        Ok(updated > 0)
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
