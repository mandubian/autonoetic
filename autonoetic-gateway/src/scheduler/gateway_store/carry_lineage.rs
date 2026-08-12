//! Federation carry-forward lineage (#1067 follow-up): `carry_forward_lineage`
//! records, for each accepted carry, the edge from the artifact that received
//! the carried verdict to the prior artifact whose gate verdict was reused.
//!
//! Verification of a carry is anchored to the planner naming the prior
//! artifact within the same workflow plus the content-addressed digest match
//! (`federation_carry_forward.rs::verify_carry_claim`). This table makes the
//! resulting lineage answerable from the store: which artifacts a given
//! artifact carried from, and the full ancestry chain back to the root of the
//! lineage. It is written only after a carry has been mechanically accepted —
//! rejected claims never leave a row.

use super::GatewayStore;
use anyhow::Result;
use rusqlite::params;

/// A row of `carry_forward_lineage`: artifact `artifact_id` received a carried
/// verdict in `role` from `source_artifact_id` (the prior artifact, referenced
/// by the planner as `source_artifact_ref`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarryLineageRecord {
    pub artifact_id: String,
    pub role: String,
    pub source_artifact_id: String,
    pub source_artifact_ref: String,
    pub strictness: String,
    pub source_code_digest: Option<String>,
    pub source_contract_digest: Option<String>,
    pub verified_at: String,
}

fn carry_lineage_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CarryLineageRecord> {
    Ok(CarryLineageRecord {
        artifact_id: row.get(0)?,
        role: row.get(1)?,
        source_artifact_id: row.get(2)?,
        source_artifact_ref: row.get(3)?,
        strictness: row.get(4)?,
        source_code_digest: row.get(5)?,
        source_contract_digest: row.get(6)?,
        verified_at: row.get(7)?,
    })
}

const CARRY_LINEAGE_COLUMNS: &str =
    "artifact_id, role, source_artifact_id, source_artifact_ref, strictness, \
     source_code_digest, source_contract_digest, verified_at";

const MAX_CARRY_DEPTH: usize = 16;

impl GatewayStore {
    /// Record that `artifact_id` accepted a carried verdict in `role` from the
    /// prior artifact `source_artifact_id` (planner-declared
    /// `source_artifact_ref`), under `strictness`. Callers invoke this only
    /// after `verify_carry_claim` accepted the carry — never for rejected
    /// claims. `INSERT OR REPLACE`: re-carrying the same role onto the same
    /// artifact overwrites the earlier edge.
    pub fn record_carry_lineage(
        &self,
        artifact_id: &str,
        role: &str,
        source_artifact_id: &str,
        source_artifact_ref: &str,
        strictness: &str,
        source_code_digest: Option<&str>,
        source_contract_digest: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO carry_forward_lineage
                (artifact_id, role, source_artifact_id, source_artifact_ref, strictness,
                 source_code_digest, source_contract_digest, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                artifact_id,
                role,
                source_artifact_id,
                source_artifact_ref,
                strictness,
                source_code_digest,
                source_contract_digest,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// All carry edges for `artifact_id` (the artifacts it carried verdicts
    /// from), oldest first. Ties on `verified_at` (second-precision RFC3339 —
    /// same-tick inserts of multiple roles in one escalate) are broken by
    /// `role`, making the order total since (artifact_id, role) is the PK.
    pub fn get_carry_lineage(&self, artifact_id: &str) -> Result<Vec<CarryLineageRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CARRY_LINEAGE_COLUMNS} FROM carry_forward_lineage
             WHERE artifact_id = ?1
             ORDER BY verified_at ASC, role ASC"
        ))?;
        let rows = stmt.query_map(params![artifact_id], carry_lineage_record_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// All edges pointing AT `artifact_id` (artifacts that carried from it),
    /// oldest first (ties broken by `artifact_id` then `role` — the
    /// tie-breaker must span children, so `role` alone is insufficient here,
    /// unlike `get_carry_lineage` where the artifact is fixed).
    pub fn list_carry_edges_from(&self, source_artifact_id: &str) -> Result<Vec<CarryLineageRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CARRY_LINEAGE_COLUMNS} FROM carry_forward_lineage
             WHERE source_artifact_id = ?1
             ORDER BY verified_at ASC, artifact_id ASC, role ASC"
        ))?;
        let rows = stmt.query_map(params![source_artifact_id], carry_lineage_record_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Walk the carry ancestry chain from `artifact_id` backwards: for each
    /// carry edge the artifact accepted, follow the source artifact's own
    /// edges, up to `MAX_CARRY_DEPTH` hops. Cycle-guarded. Returns the edges
    /// in walk order, nearest first — the chain ends at the first artifact
    /// with no recorded carries (the original gate runs).
    pub fn walk_carry_ancestors(&self, artifact_id: &str) -> Result<Vec<CarryLineageRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut cursor = artifact_id.to_string();
        for _ in 0..MAX_CARRY_DEPTH {
            let mut stmt = conn.prepare(&format!(
                "SELECT {CARRY_LINEAGE_COLUMNS} FROM carry_forward_lineage
                 WHERE artifact_id = ?1
                 ORDER BY verified_at ASC, role ASC"
            ))?;
            let rows = stmt
                .query_map(params![&cursor], carry_lineage_record_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.is_empty() {
                break;
            }
            // An artifact can carry from multiple roles/sources. The walk
            // follows the oldest edge only — one path to the root, mirroring
            // fork_ancestor_roots — and emits every edge of each visited
            // artifact alongside it. Cycle guard checked BEFORE emitting,
            // like fork_ancestor_roots: a revisit terminates the walk without
            // re-emitting the revisiting edge.
            let next = rows[0].source_artifact_id.clone();
            if !visited.insert(next.clone()) {
                break;
            }
            for row in &rows {
                out.push(row.clone());
            }
            cursor = next;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_store() -> (tempfile::TempDir, GatewayStore) {
        let temp = tempdir().unwrap();
        let store = GatewayStore::open(temp.path()).unwrap();
        (temp, store)
    }

    fn record(
        store: &GatewayStore,
        artifact_id: &str,
        role: &str,
        source: &str,
        source_ref: &str,
    ) {
        store
            .record_carry_lineage(
                artifact_id,
                role,
                source,
                source_ref,
                "conservative",
                Some("sha256:code"),
                Some("sha256:contract"),
            )
            .unwrap();
    }

    /// Roundtrip: a carry edge is recorded and read back with provenance.
    /// Ordering is total — (verified_at, role) — and (artifact_id, role) is
    /// the PK, so positional assertions are stable.
    #[test]
    fn record_and_get_carry_lineage() {
        let (_temp, store) = open_store();
        record(&store, "art_child", "unit_test_runner", "art_prior", "ar.prior1");
        record(&store, "art_child", "auditor", "art_prior", "ar.prior1");

        let rows = store.get_carry_lineage("art_child").unwrap();
        assert_eq!(rows.len(), 2, "one row per carried role");
        assert_eq!(rows[0].role, "unit_test_runner", "inserted first, earlier ts");
        assert_eq!(rows[1].role, "auditor");
        for row in &rows {
            assert_eq!(row.artifact_id, "art_child");
            assert_eq!(row.source_artifact_id, "art_prior");
            assert_eq!(row.source_artifact_ref, "ar.prior1");
            assert_eq!(row.strictness, "conservative");
            assert_eq!(row.source_code_digest.as_deref(), Some("sha256:code"));
            assert_eq!(
                row.source_contract_digest.as_deref(),
                Some("sha256:contract")
            );
            assert!(!row.verified_at.is_empty());
        }
        // Reverse lookup: edges pointing at the source.
        let pointing = store.list_carry_edges_from("art_prior").unwrap();
        assert_eq!(pointing.len(), 2);
        assert!(store.get_carry_lineage("art_prior").unwrap().is_empty());
    }

    /// A re-carry of the same role onto the same artifact replaces the edge.
    #[test]
    fn same_role_recarry_replaces_edge() {
        let (_temp, store) = open_store();
        record(&store, "art_child", "auditor", "art_prior1", "ar.p1");
        record(&store, "art_child", "auditor", "art_prior2", "ar.p2");

        let rows = store.get_carry_lineage("art_child").unwrap();
        assert_eq!(rows.len(), 1, "PK (artifact_id, role) — re-carry replaces");
        assert_eq!(rows[0].source_artifact_id, "art_prior2");
    }

    /// Multi-hop chain: A was run fresh, B carried from A, C carried from B.
    /// Walking from C reaches B then A; the chain ends at A (no edge on A).
    #[test]
    fn walk_carry_ancestors_follows_chain() {
        let (_temp, store) = open_store();
        record(&store, "art_b", "unit_test_runner", "art_a", "ar.a");
        record(&store, "art_c", "unit_test_runner", "art_b", "ar.b");
        record(&store, "art_c", "auditor", "art_b", "ar.b");

        let chain = store.walk_carry_ancestors("art_c").unwrap();
        assert_eq!(chain.len(), 3, "C's own edges (both roles), then B's edge");
        assert_eq!(chain[0].artifact_id, "art_c");
        assert_eq!(chain[0].source_artifact_id, "art_b");
        assert_eq!(chain[1].artifact_id, "art_c");
        assert_eq!(chain[1].source_artifact_id, "art_b");
        assert_eq!(chain[2].artifact_id, "art_b");
        assert_eq!(chain[2].source_artifact_id, "art_a");
    }

    /// A self-referencing edge (corrupt data) must not loop forever.
    #[test]
    fn walk_carry_ancestors_cycle_guard() {
        let (_temp, store) = open_store();
        record(&store, "art_loop", "auditor", "art_loop", "ar.self");

        let chain = store.walk_carry_ancestors("art_loop").unwrap();
        assert_eq!(chain.len(), 1, "cycle terminates on revisit");
    }

    /// An artifact with no carry edges has an empty ancestry walk.
    #[test]
    fn walk_carry_ancestors_empty_for_fresh_artifact() {
        let (_temp, store) = open_store();
        assert!(store.walk_carry_ancestors("art_fresh").unwrap().is_empty());
    }

    /// Genuine same-timestamp ties (e.g. a row written with an identical
    /// RFC3339 `verified_at`) break deterministically by `role` — the
    /// ORDER BY tie-breaker Copilot review asked for.
    #[test]
    fn same_timestamp_ties_break_by_role() {
        let (_temp, store) = open_store();
        {
            let conn = store.conn.lock().unwrap();
            for role in ["unit_test_runner", "auditor"] {
                conn.execute(
                    "INSERT INTO carry_forward_lineage
                        (artifact_id, role, source_artifact_id, source_artifact_ref,
                         strictness, verified_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        "art_child",
                        role,
                        "art_prior",
                        "ar.prior1",
                        "conservative",
                        "2026-01-01T00:00:00Z",
                    ],
                )
                .unwrap();
            }
        }

        let rows = store.get_carry_lineage("art_child").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].role, "auditor", "identical timestamps break by role");
        assert_eq!(rows[1].role, "unit_test_runner");
    }
}