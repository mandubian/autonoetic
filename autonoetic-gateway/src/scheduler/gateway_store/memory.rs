use super::memory_object_from_row;
use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::memory::MemoryObject;
use rusqlite::params;

impl GatewayStore {
    // --- Tier 2 memories (gateway.db) ---

    pub fn memory_upsert(&self, memory: &MemoryObject) -> Result<()> {
        let tags_json = serde_json::to_string(&memory.tags)?;
        let lineage_json = serde_json::to_string(&memory.lineage)?;

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO memories (
                memory_id, scope, owner_agent_id, writer_agent_id, source_type, source_ref,
                created_at, updated_at, content, content_hash, confidence, tags, lineage,
                visibility, expires_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                &memory.memory_id,
                &memory.scope,
                &memory.owner_agent_id,
                &memory.writer_agent_id,
                serde_json::to_string(&memory.source_type)?,
                &memory.source_ref,
                &memory.created_at,
                &memory.updated_at,
                &memory.content,
                &memory.content_hash,
                memory.confidence,
                tags_json,
                lineage_json,
                serde_json::to_string(&memory.visibility)?,
                &memory.expires_at,
            ],
        )?;
        tx.execute(
            "DELETE FROM memory_tags WHERE memory_id = ?1",
            params![&memory.memory_id],
        )?;
        for raw in &memory.tags {
            let t = raw.trim();
            if t.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO memory_tags (memory_id, scope, tag) VALUES (?1, ?2, ?3)",
                params![&memory.memory_id, &memory.scope, t],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn memory_get_unrestricted(&self, memory_id: &str) -> Result<Option<MemoryObject>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM memories WHERE memory_id = ?1")?;
        let mut rows = stmt.query(params![memory_id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(memory_object_from_row(&row)?))
    }

    pub fn memory_list_ids_for_scope(
        &self,
        scope: &str,
        content_substr: Option<&str>,
    ) -> Result<Vec<String>> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT memory_id FROM memories WHERE scope = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
        );
        if content_substr.is_some() {
            sql.push_str(" AND content LIKE ?3");
        }
        sql.push_str(" ORDER BY updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = match content_substr {
            Some(q) => {
                let term = format!("%{}%", q);
                stmt.query(params![scope, &now, term])?
            }
            None => stmt.query(params![scope, &now])?,
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    /// Returns IDs of memories in `scope` that are readable by `agent_id` and match all `tags`.
    /// Optional `content_substr` applies `LIKE %substr%` on content. Results are sorted by
    /// recency and capped by `limit`.
    pub fn memory_list_ids_matching_tags(
        &self,
        scope: &str,
        agent_id: &str,
        reader_session_id: Option<&str>,
        tags: &[String],
        content_substr: Option<&str>,
        limit: i64,
    ) -> Result<Vec<String>> {
        use rusqlite::types::Value;
        use std::collections::BTreeSet;

        let mut norm: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for t in tags {
            let s = t.trim();
            if s.is_empty() {
                continue;
            }
            if seen.insert(s.to_string()) {
                norm.push(s.to_string());
            }
        }
        if norm.is_empty() {
            anyhow::bail!("tags must contain at least one non-empty tag after trimming");
        }
        if limit <= 0 {
            anyhow::bail!("limit must be positive");
        }

        let now = chrono::Utc::now().to_rfc3339();
        let sess = reader_session_id.unwrap_or("").to_string();
        let mut sql = String::from("SELECT m.memory_id FROM memories m WHERE m.scope = ?1 ");
        sql.push_str("AND (m.expires_at IS NULL OR m.expires_at > ?2) ");
        sql.push_str(
            "AND (
                json_extract(m.visibility, '$.kind') = 'global'
                OR json_extract(m.visibility, '$') = 'global'
                OR json_extract(m.visibility, '$') = 'shared'
                OR (
                    (json_extract(m.visibility, '$.kind') = 'private'
                     OR json_extract(m.visibility, '$') = 'private')
                    AND (m.owner_agent_id = ?3 OR m.writer_agent_id = ?3)
                )
                OR (
                    json_extract(m.visibility, '$.kind') = 'session'
                    AND (
                        m.owner_agent_id = ?3
                        OR m.writer_agent_id = ?3
                        OR (?4 != ''
                            AND json_extract(m.visibility, '$.session_id') = ?4)
                    )
                )
            ) ",
        );

        let mut next_param: i32 = 5;
        if content_substr.is_some() {
            sql.push_str(&format!("AND m.content LIKE ?{} ", next_param));
            next_param += 1;
        }
        for _ in &norm {
            sql.push_str(&format!(
                "AND EXISTS (
                    SELECT 1 FROM memory_tags mt
                    WHERE mt.memory_id = m.memory_id
                      AND mt.scope = ?1
                      AND mt.tag = ?{}
                ) ",
                next_param
            ));
            next_param += 1;
        }
        sql.push_str(&format!("ORDER BY m.updated_at DESC LIMIT ?{}", next_param));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let mut bind: Vec<Value> = vec![
            Value::Text(scope.to_string()),
            Value::Text(now),
            Value::Text(agent_id.to_string()),
            Value::Text(sess),
        ];
        if let Some(q) = content_substr {
            bind.push(Value::Text(format!("%{}%", q)));
        }
        for t in norm {
            bind.push(Value::Text(t));
        }
        bind.push(Value::Integer(limit));

        let mut rows = stmt.query(rusqlite::params_from_iter(bind.iter()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    pub fn memory_list_ids_owned_by(&self, owner_agent_id: &str) -> Result<Vec<String>> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT memory_id FROM memories WHERE owner_agent_id = ?1
             AND (expires_at IS NULL OR expires_at > ?2) ORDER BY created_at DESC",
        )?;
        let mut rows = stmt.query(params![owner_agent_id, &now])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    pub fn memory_list_scopes_for_agent(
        &self,
        agent_id: &str,
        reader_session_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let now = chrono::Utc::now().to_rfc3339();
        let sess = reader_session_id.unwrap_or("").to_string();
        const LIST_SCOPES_SQL: &str = r#"
            SELECT DISTINCT scope FROM memories
            WHERE (expires_at IS NULL OR expires_at > ?2)
              AND (
                json_extract(visibility, '$.kind') = 'global'
                OR json_extract(visibility, '$') = 'global'
                OR json_extract(visibility, '$') = 'shared'
                OR (
                    (json_extract(visibility, '$.kind') = 'private'
                     OR json_extract(visibility, '$') = 'private')
                    AND (owner_agent_id = ?1 OR writer_agent_id = ?1)
                )
                OR (
                    json_extract(visibility, '$.kind') = 'session'
                    AND (
                        owner_agent_id = ?1
                        OR writer_agent_id = ?1
                        OR (?3 != ''
                            AND json_extract(visibility, '$.session_id') = ?3)
                    )
                )
              )
            ORDER BY scope
        "#;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(LIST_SCOPES_SQL)?;
        let mut rows = stmt.query(params![agent_id, &now, &sess])?;
        let mut scopes = Vec::new();
        while let Some(row) = rows.next()? {
            scopes.push(row.get(0)?);
        }
        Ok(scopes)
    }
}
