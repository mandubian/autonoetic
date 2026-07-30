//! Artifact egress labels — RFC data-envelopes §4.5 (artifact birth point), #980.
//!
//! An artifact's label is the intersection of the taints of everything that went
//! into it. Recorded here, keyed by `artifact_id`, at build time; read back when
//! the artifact's content re-enters a session (inspect / read / mount), so
//! content that was labeled when it was produced stays labeled after a round
//! trip through the content-addressed store.
//!
//! **Why a sidecar and not `manifest.json`.** Artifacts are content-addressed:
//! identical bytes dedup to one `artifact_id` regardless of who built them. A
//! label in the manifest would have to be tightened in place on reuse, changing
//! `artifact_manifest_digest` — which is pinned in `ArtifactRefRecord` and
//! verified on read, so existing refs would start failing as tampering. The
//! immutable manifest stays immutable; the mutable policy metadata lives here.
//!
//! Only *restrictive* labels are stored (absence ⇒ unrestricted), and the write
//! path only ever intersects, so an artifact's label can tighten but never widen
//! (RFC §2.4).

use anyhow::Result;
use autonoetic_types::egress::EgressLabel;
use rusqlite::{params, Connection, OptionalExtension};

/// Read an artifact's label. `None` ⇒ unrestricted.
pub(super) fn get_label(conn: &Connection, artifact_id: &str) -> Result<Option<EgressLabel>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT label_json FROM artifact_egress_labels WHERE artifact_id = ?1",
            params![artifact_id],
            |row| row.get(0),
        )
        .optional()?;
    match row {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

/// Intersect `label` into an artifact's stored label and return the result.
///
/// Never widens: a later build of the same content by a *cleaner* session cannot
/// relax a label an earlier tainted build established. Content-addressed dedup
/// means the two builds are the same bytes, and if any producer was tainted those
/// bytes are tainted — so intersecting is the fail-closed reading. The cost is
/// accepted over-restriction when two unrelated sessions happen to produce
/// byte-identical content.
///
/// An unrestricted result stores nothing (and clears any existing row), keeping
/// "absence ⇒ unrestricted" true in the table rather than by convention.
pub(super) fn restrict_label(
    conn: &Connection,
    artifact_id: &str,
    label: &EgressLabel,
) -> Result<EgressLabel> {
    let merged = match get_label(conn, artifact_id)? {
        Some(existing) => existing.restrict(label),
        None => label.clone(),
    };
    if merged.is_unrestricted() {
        conn.execute(
            "DELETE FROM artifact_egress_labels WHERE artifact_id = ?1",
            params![artifact_id],
        )?;
        return Ok(merged);
    }
    conn.execute(
        "INSERT INTO artifact_egress_labels (artifact_id, label_json, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(artifact_id) DO UPDATE SET label_json = ?2, updated_at = ?3",
        params![
            artifact_id,
            serde_json::to_string(&merged)?,
            chrono::Utc::now().to_rfc3339()
        ],
    )?;
    Ok(merged)
}
