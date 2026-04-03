//! Bootstrap agents from the agents directory into the gateway store.
//!
//! Scans `config.agents_dir` for agent bundles (directories with `SKILL.md`),
//! creates revisions from their content, and auto-promotes them. Skips agents
//! that already have revisions.

use anyhow::Result;
use autonoetic_types::agent_revision::{AgentRevisionRecord, AgentRevisionStatus};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::id_format::mint_hashed_prefixed_id;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

/// Bootstrap all agents from `config.agents_dir` into the gateway store.
/// Returns the number of agents activated.
pub fn bootstrap_agents(config: &GatewayConfig, gateway_dir: &Path) -> Result<usize> {
    let db_path = gateway_dir.join("gateway.db");
    let conn = Mutex::new(Connection::open(&db_path)?);
    let mut activated = 0usize;

    for entry in std::fs::read_dir(&config.agents_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let agent_dir = entry.path();
        let skill_path = agent_dir.join("SKILL.md");
        if !skill_path.exists() {
            continue;
        }
        let agent_id = agent_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid agent dir name: {}", agent_dir.display()))?
            .to_string();

        // Skip if the agent already has revisions
        {
            let conn = conn.lock().unwrap();
            let existing = list_agent_revisions(&conn, &agent_id)?;
            if !existing.is_empty() {
                continue;
            }
        }

        let skill_content = std::fs::read(&skill_path)?;
        let skill_text = String::from_utf8_lossy(&skill_content);
        let (parsed_manifest, _instructions) =
            crate::runtime::parser::SkillParser::parse(&skill_text).map_err(|e| {
                anyhow::anyhow!("Failed to parse SKILL.md for '{}': {}", agent_id, e)
            })?;

        let lock_rel_path = &parsed_manifest.runtime.runtime_lock;
        let lock_path = agent_dir.join(lock_rel_path);
        let lock_content = std::fs::read(&lock_path).map_err(|e| {
            anyhow::anyhow!(
                "Missing runtime.lock '{}' for agent '{}': {}",
                lock_rel_path,
                agent_id,
                e
            )
        })?;

        let manifest_hash = format!("sha256:{:x}", Sha256::digest(&skill_content));
        let runtime_lock_hash = format!("sha256:{:x}", Sha256::digest(&lock_content));

        let mut file_map: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        collect_files(&agent_dir, &agent_dir, &mut file_map)?;
        file_map.insert(lock_rel_path.clone(), lock_content.clone());

        let mut hasher = Sha256::new();
        for (path, bytes) in &file_map {
            hasher.update(path.as_bytes());
            hasher.update([0_u8]);
            hasher.update(bytes);
            hasher.update([0_u8]);
        }
        let revision_digest_hex = format!("{:x}", hasher.finalize());
        let revision_id = format!("rev_sha256:{}", revision_digest_hex);
        let content_digest = format!("sha256:{}", revision_digest_hex);

        // Skip if this exact revision already exists
        {
            let conn = conn.lock().unwrap();
            if get_agent_revision(&conn, &revision_id)?.is_some() {
                continue;
            }
        }

        let revision_dir = gateway_dir
            .join("revisions")
            .join("agents")
            .join(&agent_id)
            .join(&revision_id);

        if !revision_dir.exists() {
            for (rel_path, bytes) in &file_map {
                let dest = revision_dir.join(rel_path);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, bytes)?;
            }
        }

        let now = chrono::Utc::now().to_rfc3339();
        let rev = AgentRevisionRecord {
            revision_id: revision_id.clone(),
            agent_id: agent_id.clone(),
            base_revision_id: None,
            artifact_id: None,
            content_digest,
            runtime_lock_hash,
            manifest_hash,
            created_at: now.clone(),
            created_by_type: "bootstrap".to_string(),
            created_by_id: "cli".to_string(),
            source_kind: "bootstrap".to_string(),
            source_ref: None,
            origin_node_id: config.node_id.clone(),
            trust_domain: "local".to_string(),
            status: AgentRevisionStatus::Candidate,
            metadata_json: serde_json::json!({
                "summary": "Bootstrapped from reference agent bundle",
            }),
            short_id: String::new(),
        };

        {
            let mut conn = conn.lock().unwrap();
            insert_agent_revision_transactional(&mut conn, &rev)?;

            let promotion_id =
                mint_hashed_prefixed_id("prom-", &format!("{}-{}-{}", agent_id, revision_id, now));

            atomic_promote(
                &mut conn,
                &agent_id,
                &revision_id,
                &promotion_id,
                "bootstrap",
                "cli",
                Some("Auto-promoted during agent bootstrap"),
                None,
            )?;
        }

        activated += 1;
    }

    Ok(activated)
}

fn collect_files(base: &Path, current: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(base)
            .map_err(|e| anyhow::anyhow!("Failed to compute relative path: {}", e))?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(&path)?;
        out.insert(rel, bytes);
    }
    Ok(())
}

fn list_agent_revisions(conn: &Connection, agent_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT revision_id FROM agent_revisions WHERE agent_id = ?1")?;
    let rows = stmt.query_map(params![agent_id], |row| row.get(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn get_agent_revision(conn: &Connection, revision_id: &str) -> Result<Option<AgentRevisionRecord>> {
    let row = conn.query_row(
        "SELECT revision_id, agent_id, base_revision_id, artifact_id, content_digest,
                runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
                source_kind, source_ref, origin_node_id, trust_domain, status,
                metadata_json, short_id
         FROM agent_revisions WHERE revision_id = ?1",
        params![revision_id],
        |row| {
            let metadata_json: String = row.get(15)?;
            let status_str: String = row.get(14)?;
            let status: AgentRevisionStatus = serde_json::from_str(&status_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    e.to_string().into(),
                )
            })?;
            Ok(AgentRevisionRecord {
                revision_id: row.get(0)?,
                agent_id: row.get(1)?,
                base_revision_id: row.get(2)?,
                artifact_id: row.get(3)?,
                content_digest: row.get(4)?,
                runtime_lock_hash: row.get(5)?,
                manifest_hash: row.get(6)?,
                created_at: row.get(7)?,
                created_by_type: row.get(8)?,
                created_by_id: row.get(9)?,
                source_kind: row.get(10)?,
                source_ref: row.get(11)?,
                origin_node_id: row.get(12)?,
                trust_domain: row.get(13)?,
                status,
                metadata_json: serde_json::from_str(&metadata_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        15,
                        rusqlite::types::Type::Text,
                        e.to_string().into(),
                    )
                })?,
                short_id: row.get(16)?,
            })
        },
    );
    match row {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn insert_agent_revision_transactional(
    conn: &mut Connection,
    rev: &AgentRevisionRecord,
) -> Result<()> {
    let metadata_json = serde_json::to_string(&rev.metadata_json)?;
    let tx = conn.transaction()?;

    let short = if rev.short_id.trim().is_empty() {
        let mut stmt =
            tx.prepare("SELECT revision_id FROM agent_revisions WHERE revision_id != ?1")?;
        let rows = stmt.query_map(params![&rev.revision_id], |row| row.get::<_, String>(0))?;
        let mut existing = Vec::new();
        for row in rows {
            existing.push(row?);
        }
        autonoetic_types::agent_revision::short_id_unique(
            &rev.revision_id,
            existing.iter().map(|s| s.as_str()),
            None,
        )
    } else {
        rev.short_id.clone()
    };

    tx.execute(
        "INSERT INTO agent_revisions (
            revision_id, agent_id, base_revision_id, artifact_id, content_digest,
            runtime_lock_hash, manifest_hash, created_at, created_by_type, created_by_id,
            source_kind, source_ref, origin_node_id, trust_domain, status,
            metadata_json, short_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            &rev.revision_id,
            &rev.agent_id,
            &rev.base_revision_id,
            &rev.artifact_id,
            &rev.content_digest,
            &rev.runtime_lock_hash,
            &rev.manifest_hash,
            &rev.created_at,
            &rev.created_by_type,
            &rev.created_by_id,
            &rev.source_kind,
            &rev.source_ref,
            &rev.origin_node_id,
            &rev.trust_domain,
            serde_json::to_string(&rev.status)?,
            metadata_json,
            &short,
        ],
    )?;

    tx.commit()?;
    Ok(())
}

fn atomic_promote(
    conn: &mut Connection,
    agent_id: &str,
    revision_id: &str,
    promotion_id: &str,
    promoted_by_type: &str,
    promoted_by_id: &str,
    reason: Option<&str>,
    eval_run_id: Option<&str>,
) -> Result<Option<String>> {
    let tx = conn.transaction()?;

    let prev_alias_row: Option<String> = tx
        .query_row(
            "SELECT revision_id FROM agent_aliases WHERE alias = ?1",
            params![agent_id],
            |row| row.get(0),
        )
        .optional()?;

    let now = chrono::Utc::now().to_rfc3339();
    tx.execute(
        "INSERT OR REPLACE INTO agent_aliases (alias, revision_id, updated_at) VALUES (?1, ?2, ?3)",
        params![agent_id, revision_id, now],
    )?;

    let promotion_kind = if prev_alias_row.is_none() {
        "initial"
    } else {
        "replacement"
    };

    tx.execute(
        "INSERT INTO agent_promotions (
            promotion_id, agent_id, revision_id, previous_revision_id,
            promoted_at, promoted_by_type, promoted_by_id, reason, kind, eval_run_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            promotion_id,
            agent_id,
            revision_id,
            prev_alias_row,
            now,
            promoted_by_type,
            promoted_by_id,
            reason,
            promotion_kind,
            eval_run_id,
        ],
    )?;

    tx.commit()?;
    Ok(prev_alias_row)
}
