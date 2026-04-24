use autonoetic_types::memory::MemoryObject;
use rusqlite;

pub(crate) fn memory_object_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryObject> {
    let source_type_str: String = row.get(4)?;
    let tags_str: Option<String> = row.get(11)?;
    let lineage_str: Option<String> = row.get(12)?;
    let visibility_str: String = row.get(13)?;

    Ok(MemoryObject {
        memory_id: row.get(0)?,
        scope: row.get(1)?,
        owner_agent_id: row.get(2)?,
        writer_agent_id: row.get(3)?,
        source_type: serde_json::from_str(&source_type_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        })?,
        source_ref: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        content: row.get(8)?,
        content_hash: row.get(9)?,
        confidence: row.get(10)?,
        tags: serde_json::from_str(&tags_str.unwrap_or_default()).unwrap_or_default(),
        lineage: serde_json::from_str(&lineage_str.unwrap_or_default()).unwrap_or_default(),
        visibility: serde_json::from_str(&visibility_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        })?,
        expires_at: row.get(14)?,
        revision_id: row.get(15)?,
        binding_session_id: row.get(16)?,
        alias_ref: row.get(17)?,
        quarantine_reason: row.get(18)?,
    })
}
