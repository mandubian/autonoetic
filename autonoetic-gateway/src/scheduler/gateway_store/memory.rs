use super::memory_object_from_row;
use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::memory::MemoryObject;
use rusqlite::params;

/// Check if a query string contains FTS5-syntax characters that could cause
/// a MATCH parse error, in which case we fall back to LIKE.
fn looks_like_fts_syntax(query: &str) -> bool {
    query.chars().any(|c| {
        matches!(c, '.' | '(' | ')' | '"' | '*' | '-' | '+' | '&' | ':')
    })
}

fn should_fallback_to_like(err: &rusqlite::Error, query: &str) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.extended_code == rusqlite::ffi::SQLITE_ERROR && looks_like_fts_syntax(query)
    )
}

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
                visibility, expires_at, revision_id, binding_session_id, alias_ref,
                quarantine_reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
                &memory.revision_id,
                &memory.binding_session_id,
                &memory.alias_ref,
                &memory.quarantine_reason,
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

    pub fn memory_get(&self, memory_id: &str) -> Result<Option<MemoryObject>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT * FROM memories WHERE memory_id = ?1 AND quarantine_reason IS NULL")?;
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

        if let Some(q) = content_substr {
            // Try FTS5 full-text search first
            let fts_sql = String::from(
                "SELECT m.memory_id FROM memories m
                 JOIN memories_fts ON m.rowid = memories_fts.rowid
                 WHERE m.scope = ?1
                   AND (m.expires_at IS NULL OR m.expires_at > ?2)
                   AND m.quarantine_reason IS NULL
                   AND memories_fts MATCH ?3
                 ORDER BY m.updated_at DESC",
            );
            let mut stmt = conn.prepare(&fts_sql)?;
            let query_result = stmt.query(params![scope, &now, q]);
            match query_result {
                Ok(mut rows) => {
                    let mut out = Vec::new();
                    // FTS5 may defer MATCH evaluation to row iteration (rows.next()),
                    // not query() — so we must catch errors here too, not just from
                    // stmt.query(). A query like "friend-chat" is parsed by FTS5 as
                    // "friend NOT chat", which can surface as a SQL error during step.
                    let mut fts_error: Option<rusqlite::Error> = None;
                    loop {
                        match rows.next() {
                            Ok(Some(row)) => match row.get::<_, String>(0) {
                                Ok(v) => out.push(v),
                                Err(e) => return Err(e.into()),
                            },
                            Ok(None) => break,
                            Err(e) => {
                                fts_error = Some(e);
                                break;
                            }
                        }
                    }
                    match fts_error {
                        Some(e) if should_fallback_to_like(&e, q) => { /* fall through to LIKE */ }
                        Some(e) => return Err(e.into()),
                        None if !out.is_empty() || !looks_like_fts_syntax(q) => return Ok(out),
                        None => {}
                    }
                }
                Err(ref e) if should_fallback_to_like(e, q) => {}
                Err(e) => return Err(e.into()),
            }
        }

        // Fallback: LIKE substring search (or no query)
        let mut sql = String::from(
            "SELECT memory_id FROM memories WHERE scope = ?1 AND (expires_at IS NULL OR expires_at > ?2) AND quarantine_reason IS NULL",
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
    /// Optional `content_substr` applies full-text search (FTS5 MATCH) on content,
    /// falling back to `LIKE %substr%` on FTS syntax error. Results are sorted by
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
        let conn = self.conn.lock().unwrap();

        let vis_clause = "\
            (json_extract(m.visibility, '$.kind') = 'global' \
             OR json_extract(m.visibility, '$') = 'global' \
             OR json_extract(m.visibility, '$') = 'shared' \
             OR ( \
                 (json_extract(m.visibility, '$.kind') = 'private' \
                  OR json_extract(m.visibility, '$') = 'private') \
                 AND (m.owner_agent_id = ?A OR m.writer_agent_id = ?A) \
             ) \
             OR ( \
                 json_extract(m.visibility, '$.kind') = 'session' \
                 AND ( \
                     m.owner_agent_id = ?A \
                     OR m.writer_agent_id = ?A \
                     OR (?S != '' \
                         AND json_extract(m.visibility, '$.session_id') = ?S) \
                 ) \
             ) \
            )";

        fn make_tag_clauses(tags: &[String], start_param: i32) -> (String, Vec<String>) {
            let mut clauses = Vec::new();
            let mut vals = Vec::new();
            for (i, t) in tags.iter().enumerate() {
                let p = start_param + i as i32;
                clauses.push(format!(
                    "EXISTS (SELECT 1 FROM memory_tags mt WHERE mt.memory_id = m.memory_id AND mt.scope = ?1 AND mt.tag = ?{})",
                    p
                ));
                vals.push(t.clone());
            }
            (clauses.join(" AND "), vals)
        }

        // --- FTS5 path ---
        // Parameter layout: ?1=scope ?2=now ?3=query ?4=agent ?5=sess ?6..=tags ?N=limit
        if let Some(q) = content_substr {
            if !q.is_empty() {
                let (tag_sql, tag_vals) = make_tag_clauses(&norm, 6);
                let vis = vis_clause
                    .replace("?A", "?4")
                    .replace("?S", "?5");
                let fts_sql = format!(
                    "SELECT m.memory_id FROM memories m \
                     JOIN memories_fts ON m.rowid = memories_fts.rowid \
                     WHERE m.scope = ?1 \
                       AND (m.expires_at IS NULL OR m.expires_at > ?2) \
                       AND m.quarantine_reason IS NULL \
                       AND {vis} \
                       AND memories_fts MATCH ?3 \
                       AND {tag_sql} \
                     ORDER BY m.updated_at DESC LIMIT ?{limit_param}",
                    vis = vis,
                    tag_sql = tag_sql,
                    limit_param = 6 + norm.len() as i32,
                );

                let mut stmt = match conn.prepare(&fts_sql) {
                    Ok(s) => s,
                    Err(e) => return Err(e.into()),
                };
                let mut bind: Vec<Value> = vec![
                    Value::Text(scope.to_string()),
                    Value::Text(now.clone()),
                    Value::Text(q.to_string()),
                    Value::Text(agent_id.to_string()),
                    Value::Text(sess.clone()),
                ];
                for t in &tag_vals {
                    bind.push(Value::Text(t.clone()));
                }
                bind.push(Value::Integer(limit));

                let fts_result = stmt.query(rusqlite::params_from_iter(bind.iter()));
                match fts_result {
                    Ok(mut rows) => {
                        let mut out = Vec::new();
                        let mut fts_error: Option<rusqlite::Error> = None;
                        loop {
                            match rows.next() {
                                Ok(Some(row)) => match row.get::<_, String>(0) {
                                    Ok(v) => out.push(v),
                                    Err(e) => return Err(e.into()),
                                },
                                Ok(None) => break,
                                Err(e) => {
                                    fts_error = Some(e);
                                    break;
                                }
                            }
                        }
                        match fts_error {
                            Some(e) if should_fallback_to_like(&e, q) => { /* fall through to LIKE */ }
                            Some(e) => return Err(e.into()),
                            None if !out.is_empty() || !looks_like_fts_syntax(q) => return Ok(out),
                            None => {}
                        }
                    }
                    Err(ref e) if should_fallback_to_like(e, q) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // --- LIKE fallback path ---
        // Parameter layout: ?1=scope ?2=now ?3=agent ?4=sess ?5=content_substr(if present) ?6../=tags ?N=limit
        let like_offset: i32 = if content_substr.is_some() { 1 } else { 0 };
        let content_param = 5;
        let tag_start = content_param + like_offset;
        let (tag_sql, tag_vals) = make_tag_clauses(&norm, tag_start);
        let vis = vis_clause
            .replace("?A", "?3")
            .replace("?S", "?4");
        let mut sql = format!(
            "SELECT m.memory_id FROM memories m \
             WHERE m.scope = ?1 \
               AND (m.expires_at IS NULL OR m.expires_at > ?2) \
               AND m.quarantine_reason IS NULL \
               AND {vis}",
            vis = vis,
        );
        if content_substr.is_some() {
            sql.push_str(&format!(" AND m.content LIKE ?{} ", content_param));
        }
        sql.push_str(&format!(
            " AND {tag_sql} ORDER BY m.updated_at DESC LIMIT ?{limit_param}",
            tag_sql = tag_sql,
            limit_param = tag_start + norm.len() as i32,
        ));

        let mut stmt = conn.prepare(&sql)?;
        let mut bind: Vec<Value> = vec![
            Value::Text(scope.to_string()),
            Value::Text(now),
            Value::Text(agent_id.to_string()),
            Value::Text(sess),
        ];
        if let Some(q) = content_substr {
            if !q.is_empty() {
                bind.push(Value::Text(format!("%{}%", q)));
            }
        }
        for t in tag_vals {
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

    /// Retrieve recent global memories matching any of the given tags.
    ///
    /// Used by the session continuity feature to prime agent context with
    /// relevant prior knowledge. Returns full `MemoryObject`s (not just IDs).
    pub fn search_memories_by_tags(
        &self,
        tags: &[&str],
        limit: usize,
    ) -> Result<Vec<MemoryObject>> {
        let tag_clauses: Vec<String> = tags
            .iter()
            .enumerate()
            .map(|(i, _)| format!("EXISTS (SELECT 1 FROM json_each(m.tags) WHERE json_each.value = ?{})", i + 3))
            .collect();

        if tag_clauses.is_empty() {
            return Ok(Vec::new());
        }

        let sql = format!(
            "SELECT m.memory_id FROM memories m \
             WHERE (json_extract(m.visibility, '$.kind') = 'global' \
                    OR json_extract(m.visibility, '$') = 'global') \
             AND (m.expires_at IS NULL OR m.expires_at > ?1) \
             AND m.quarantine_reason IS NULL \
             AND ({}) \
             ORDER BY m.created_at DESC LIMIT ?2",
            tag_clauses.join(" OR ")
        );

        let ids: Vec<String> = {
            let conn = self.conn.lock().unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let mut stmt = conn.prepare(&sql)?;
            let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            param_values.push(Box::new(now));
            param_values.push(Box::new(limit as i64));
            for tag in tags {
                param_values.push(Box::new(tag.to_string()));
            }
            let refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();
            let mut rows = stmt.query(refs.as_slice())?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                ids.push(row.get(0)?);
            }
            ids
        };

        let mut out = Vec::new();
        for id in ids {
            if let Some(obj) = self.memory_get_unrestricted(&id)? {
                out.push(obj);
            }
        }
        Ok(out)
    }

    pub fn memory_list_ids_owned_by(&self, owner_agent_id: &str) -> Result<Vec<String>> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT memory_id FROM memories WHERE owner_agent_id = ?1
             AND (expires_at IS NULL OR expires_at > ?2) AND quarantine_reason IS NULL ORDER BY created_at DESC",
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
              AND quarantine_reason IS NULL
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

    pub fn memory_quarantine_by_revision(&self, revision_id: &str, reason: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "UPDATE memories SET quarantine_reason = ?1 WHERE revision_id = ?2 AND quarantine_reason IS NULL",
            params![reason, revision_id],
        )?;
        Ok(count)
    }

    pub fn memory_list_quarantined_for_revision(
        &self,
        revision_id: &str,
    ) -> Result<Vec<MemoryObject>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM memories WHERE revision_id = ?1 AND quarantine_reason IS NOT NULL ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![revision_id], |row| memory_object_from_row(row))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }
}
