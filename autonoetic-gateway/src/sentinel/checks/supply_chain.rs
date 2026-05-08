//! Supply-chain auditing checks (Phase 4, deterministic).
//!
//! ## Check 1 — Layer scope violations
//!
//! Scans `approvals` for granted (`status = 'approved'`) `layer_mount` actions
//! that carry a non-empty `unapproved_delta`. Each delta entry means the layer
//! was built with network access to hosts that were not covered by the mounting
//! session's grants at approval time. The operator explicitly approved the
//! expansion, so this is not a block — it is an auditable finding.
//!
//! Severity: `critical` when the layer originates from `runtime.lock` (embedded
//! in the agent definition); `warning` for artifact layers.
//!
//! ## Check 2 — Layer provenance gaps
//!
//! For each layer referenced by an approved `layer_mount`, checks whether any
//! `execution_traces` row with `tool_name='sandbox_exec'` and `success=1` has a
//! `result` JSON payload whose `captured_layers` array references that `layer_id`.
//! If no such trace exists, the layer has no verifiable capture history in this
//! gateway — a `SupplyChainProvenanceGap` finding is emitted.
//!
//! Both checks use `Reproducibility::Deterministic` — they query recorded facts.

use anyhow::Result;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding,
};
use rusqlite::{params, Connection};
use serde::Deserialize;

// ── JSON payload shapes ───────────────────────────────────────────────────────

/// Mirrors the `layers` array element written by sandbox.rs when requesting a
/// `layer_mount` approval (shape of `LayerMountScopeInfo` serialised to JSON).
#[derive(Debug, Deserialize)]
struct LayerApprovalEntry {
    #[serde(default)]
    layer_id: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    unapproved_delta: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LayerMountPayload {
    #[serde(default)]
    layers: Vec<LayerApprovalEntry>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract a content-addressed artifact ID from the `source` field when it is
/// in the form `"artifact:<id>"`, returning `None` for `"runtime.lock"` or
/// other non-artifact sources.
fn artifact_id_from_source(source: &str) -> Option<String> {
    source.strip_prefix("artifact:").map(|s| s.to_string())
}

// ── Check 1: scope violations ─────────────────────────────────────────────────

/// Scan approved `layer_mount` approvals for layers whose build-time scope was
/// not subsumed by the mounting session's grants. Each approved scope expansion
/// is audited as a `SupplyChainScopeViolation` finding.
///
/// Severity:
/// - `critical` when `source = "runtime.lock"` (scope embedded in agent definition)
/// - `warning` for artifact layers (scope was explicitly approved for a single mount)
///
/// `scope_agent_id` filters to a single mounting agent — used by the pre-promotion
/// gate so a layer-scope finding from agent A does not block promotion of agent B.
/// The filter keys on the *mounting* agent (`approvals.agent_id`), not the layer's
/// originating agent, since findings are attributed to and remediated by whoever
/// approved the mount.
pub fn scan_layer_scope_violations(
    conn: &Connection,
    sentinel_revision_id: &str,
    since: Option<&str>,
    scan_limit: u32,
    scope_agent_id: Option<&str>,
) -> Result<Vec<SecurityFinding>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, agent_id, session_id, root_session_id, action_payload, decided_at
         FROM approvals
         WHERE action_type = 'layer_mount'
           AND status = 'approved'
           AND (?1 IS NULL OR decided_at >= ?1)
           AND (?3 IS NULL OR agent_id = ?3)
         ORDER BY decided_at DESC, request_id
         LIMIT ?2",
    )?;

    let rows: Vec<(String, String, String, Option<String>, String, Option<String>)> = stmt
        .query_map(params![since, scan_limit as i64, scope_agent_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();

    for (request_id, agent_id, session_id, root_session_id, action_payload, _decided_at) in rows {
        let payload: LayerMountPayload = match serde_json::from_str(&action_payload) {
            Ok(p) => p,
            Err(_) => continue,
        };

        for entry in &payload.layers {
            if entry.unapproved_delta.is_empty() {
                continue;
            }

            let is_runtime_lock = entry.source == "runtime.lock";
            let severity = if is_runtime_lock {
                FindingSeverity::Critical
            } else {
                FindingSeverity::Warning
            };

            let remediation = if is_runtime_lock {
                format!(
                    "Layer '{}' (runtime.lock, digest {}) was built with {} host(s) [{}] not in \
                     session scope. Review SKILL.md runtime.lock layers and rebuild with a \
                     narrower network scope.",
                    entry.name,
                    &entry.digest[..entry.digest.len().min(16)],
                    entry.unapproved_delta.len(),
                    entry.unapproved_delta.join(", ")
                )
            } else {
                format!(
                    "Layer '{}' (artifact, digest {}) was built with {} host(s) [{}] not in \
                     session scope. Review whether those hosts are still required and re-capture \
                     the layer with a narrower scope.",
                    entry.name,
                    &entry.digest[..entry.digest.len().min(16)],
                    entry.unapproved_delta.len(),
                    entry.unapproved_delta.join(", ")
                )
            };

            // Anchor on the layer digest (primary) and the approval record (for DB traceability).
            // For artifact sources, also anchor the content-addressed artifact ID.
            let mut anchors = vec![
                EvidenceAnchor::LayerDigest { value: entry.digest.clone() },
                EvidenceAnchor::ApprovalRecord { request_id: request_id.clone() },
            ];
            if let Some(art_id) = artifact_id_from_source(&entry.source) {
                anchors.push(EvidenceAnchor::ArtifactId { id: art_id });
            }

            let effective_session = root_session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&session_id);

            findings.push(
                SecurityFinding::new(
                    FindingType::SupplyChainScopeViolation,
                    severity,
                    1.0,
                    Reproducibility::Deterministic,
                    remediation,
                    sentinel_revision_id,
                )
                .with_affected(AffectedEntities {
                    agent_alias: Some(agent_id.clone()),
                    session_id: Some(effective_session.to_string()),
                    layer_digest: Some(entry.digest.clone()),
                    ..Default::default()
                })
                .with_anchors(anchors),
            );
        }
    }

    Ok(findings)
}

// ── Check 2: provenance gaps ──────────────────────────────────────────────────

/// Scan approved `layer_mount` approvals for layers that have no capture trace
/// in `execution_traces` (`tool_name='sandbox_exec'`, `success=1`, `result`
/// JSON referencing the layer ID in `captured_layers[*].layer_id`).
///
/// A gap means: the operator approved mounting supply-chain content that this
/// gateway cannot trace back to a known capture session. This may be benign
/// (layer built on another gateway instance) but warrants operator review.
pub fn scan_layer_provenance_gaps(
    conn: &Connection,
    sentinel_revision_id: &str,
    since: Option<&str>,
    scan_limit: u32,
    scope_agent_id: Option<&str>,
) -> Result<Vec<SecurityFinding>> {
    // Collect distinct (layer_id, digest, request_id, agent_id, session_id) from
    // approved layer_mount approvals, then filter to those with no capture trace
    // in execution_traces.result JSON (captured_layers[*].layer_id).
    //
    // `scope_agent_id` filters on the *mounting* agent so a provenance gap
    // for a layer mounted by agent A does not block promotion of agent B.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT
             json_extract(l.value, '$.layer_id')  AS layer_id,
             json_extract(l.value, '$.digest')    AS digest,
             json_extract(l.value, '$.source')    AS source,
             a.request_id, a.agent_id, a.session_id, a.root_session_id
         FROM approvals a, json_each(json_extract(a.action_payload, '$.layers')) AS l
         WHERE a.action_type = 'layer_mount'
           AND a.status = 'approved'
           AND (?1 IS NULL OR a.decided_at >= ?1)
           AND (?3 IS NULL OR a.agent_id = ?3)
           AND json_extract(l.value, '$.layer_id') IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM execution_traces et
               WHERE et.tool_name = 'sandbox_exec'
                 AND et.success = 1
                 AND et.result LIKE '%' || json_extract(l.value, '$.layer_id') || '%'
           )
         ORDER BY a.decided_at DESC, a.request_id, layer_id
         LIMIT ?2",
    )?;

    let rows: Vec<(String, String, String, String, String, String, Option<String>)> = stmt
        .query_map(
            params![since, scan_limit as i64, scope_agent_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();

    for (layer_id, digest, source, request_id, agent_id, session_id, root_session_id) in rows {
        let effective_session = root_session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&session_id);

        let remediation = format!(
            "Layer '{}' (digest {}) was approved for mounting but has no capture trace in this \
             gateway's execution_traces. Verify the layer was built under known conditions and \
             consider re-capturing it with full provenance.",
            layer_id,
            &digest[..digest.len().min(16)],
        );

        let mut anchors = vec![
            EvidenceAnchor::LayerDigest { value: digest.clone() },
            EvidenceAnchor::ApprovalRecord { request_id: request_id.clone() },
        ];
        if let Some(art_id) = artifact_id_from_source(&source) {
            anchors.push(EvidenceAnchor::ArtifactId { id: art_id });
        }

        findings.push(
            SecurityFinding::new(
                FindingType::SupplyChainProvenanceGap,
                FindingSeverity::Warning,
                0.8,
                Reproducibility::Deterministic,
                remediation,
                sentinel_revision_id,
            )
            .with_affected(AffectedEntities {
                agent_alias: Some(agent_id.clone()),
                session_id: Some(effective_session.to_string()),
                layer_digest: Some(digest),
                ..Default::default()
            })
            .with_anchors(anchors),
        );
    }

    Ok(findings)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE approvals (
                request_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                root_session_id TEXT,
                workflow_id TEXT,
                task_id TEXT,
                action_type TEXT NOT NULL,
                action_payload TEXT NOT NULL,
                reason TEXT,
                evidence_ref TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at TEXT NOT NULL,
                decided_at TEXT,
                decided_by TEXT,
                approval_level TEXT NOT NULL DEFAULT 'operator'
            );
            CREATE TABLE execution_traces (
                trace_id TEXT PRIMARY KEY,
                event_id TEXT,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                turn_id TEXT,
                timestamp TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                command TEXT,
                exit_code INTEGER,
                stdout TEXT,
                stderr TEXT,
                duration_ms INTEGER,
                success INTEGER NOT NULL,
                error_type TEXT,
                error_summary TEXT,
                approval_required INTEGER DEFAULT 0,
                approval_request_id TEXT,
                arguments TEXT,
                result TEXT
            );",
        )
        .unwrap();
        conn
    }

    fn insert_layer_mount_approval(
        conn: &Connection,
        request_id: &str,
        agent_id: &str,
        status: &str,
        layers_json: &str,
    ) {
        let payload = format!(r#"{{"layers": {layers_json}, "command": "pip install numpy"}}"#);
        conn.execute(
            "INSERT INTO approvals
                (request_id, agent_id, session_id, action_type, action_payload,
                 status, created_at, decided_at, approval_level)
             VALUES (?1, ?2, 'sess_001', 'layer_mount', ?3,
                     ?4, '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', 'operator')",
            params![request_id, agent_id, payload, status],
        )
        .unwrap();
    }

    fn insert_capture_trace(conn: &Connection, trace_id: &str, captured_layer_id: &str) {
        let result_json = format!(
            r#"{{"ok":true,"captured_layers":[{{"layer_id":"{captured_layer_id}","digest":"sha256:abc","file_count":1,"size_bytes":100}}]}}"#
        );
        conn.execute(
            "INSERT INTO execution_traces
                (trace_id, agent_id, session_id, timestamp, tool_name, success, duration_ms, result)
             VALUES (?1, 'packager.default', 'sess_build', '2026-01-01T00:00:00Z',
                     'sandbox_exec', 1, 100, ?2)",
            params![trace_id, result_json],
        )
        .unwrap();
    }

    #[test]
    fn scope_violation_flagged_for_unapproved_delta() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn, "apr-001", "coder.default", "approved",
            r#"[{"layer_id":"layer_abc","digest":"sha256:aabbcc","name":"python-deps","mount_path":"/deps","source":"artifact:art_001","build_time_approved_hosts":["pypi.org"],"unapproved_delta":["pypi.org"]}]"#,
        );
        let findings = scan_layer_scope_violations(&conn, "rev-001", None, 100, None).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert_eq!(findings[0].finding_type, FindingType::SupplyChainScopeViolation);
        // ArtifactId anchor must be present for artifact source.
        assert!(findings[0].evidence_anchors.iter().any(|a| matches!(a, EvidenceAnchor::ArtifactId { id } if id == "art_001")));
        // ApprovalRecord anchor must be present.
        assert!(findings[0].evidence_anchors.iter().any(|a| matches!(a, EvidenceAnchor::ApprovalRecord { request_id } if request_id == "apr-001")));
    }

    #[test]
    fn scope_violation_critical_for_runtime_lock_source() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn, "apr-002", "coder.default", "approved",
            r#"[{"layer_id":"layer_def","digest":"sha256:ddeeff","name":"locked-deps","mount_path":"/deps","source":"runtime.lock","build_time_approved_hosts":["private.registry.internal"],"unapproved_delta":["private.registry.internal"]}]"#,
        );
        let findings = scan_layer_scope_violations(&conn, "rev-001", None, 100, None).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Critical);
        // No ArtifactId anchor for runtime.lock source.
        assert!(!findings[0].evidence_anchors.iter().any(|a| matches!(a, EvidenceAnchor::ArtifactId { .. })));
    }

    #[test]
    fn scope_violation_skipped_when_delta_empty() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn, "apr-003", "coder.default", "approved",
            r#"[{"layer_id":"layer_clean","digest":"sha256:112233","name":"cached-deps","mount_path":"/deps","source":"artifact:art_002","build_time_approved_hosts":["pypi.org"],"unapproved_delta":[]}]"#,
        );
        let findings = scan_layer_scope_violations(&conn, "rev-001", None, 100, None).unwrap();
        assert!(findings.is_empty(), "empty delta must not produce a scope violation finding");
    }

    #[test]
    fn scope_violation_skipped_for_non_approved_status() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn, "apr-004", "coder.default", "pending",
            r#"[{"layer_id":"layer_pend","digest":"sha256:445566","name":"py","mount_path":"/deps","source":"artifact:x","build_time_approved_hosts":["pypi.org"],"unapproved_delta":["pypi.org"]}]"#,
        );
        let findings = scan_layer_scope_violations(&conn, "rev-001", None, 100, None).unwrap();
        assert!(findings.is_empty(), "pending approval must not fire");
    }

    #[test]
    fn provenance_gap_flagged_when_no_capture_trace() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn, "apr-005", "coder.default", "approved",
            r#"[{"layer_id":"layer_gap","digest":"sha256:778899","name":"unknown-origin","mount_path":"/deps","source":"artifact:art_003","build_time_approved_hosts":[],"unapproved_delta":[]}]"#,
        );
        let findings = scan_layer_provenance_gaps(&conn, "rev-001", None, 100, None).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding_type, FindingType::SupplyChainProvenanceGap);
        assert!(findings[0].evidence_anchors.iter().any(|a| matches!(a, EvidenceAnchor::ApprovalRecord { .. })));
    }

    #[test]
    fn provenance_gap_not_flagged_when_capture_trace_exists() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn, "apr-006", "coder.default", "approved",
            r#"[{"layer_id":"layer_known","digest":"sha256:aabbdd","name":"known-origin","mount_path":"/deps","source":"artifact:art_004","build_time_approved_hosts":[],"unapproved_delta":[]}]"#,
        );
        // Capture trace in execution_traces.result JSON.
        insert_capture_trace(&conn, "trace_001", "layer_known");
        let findings = scan_layer_provenance_gaps(&conn, "rev-001", None, 100, None).unwrap();
        assert!(findings.is_empty(), "layer with capture trace must not produce gap finding");
    }

    #[test]
    fn multiple_layers_in_one_approval_each_checked() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn, "apr-007", "coder.default", "approved",
            r#"[
                {"layer_id":"layer_a1","digest":"sha256:111111","name":"a","mount_path":"/a","source":"artifact:x","build_time_approved_hosts":["a.com"],"unapproved_delta":["a.com"]},
                {"layer_id":"layer_b2","digest":"sha256:222222","name":"b","mount_path":"/b","source":"artifact:y","build_time_approved_hosts":["b.com"],"unapproved_delta":[]}
            ]"#,
        );
        let violations = scan_layer_scope_violations(&conn, "rev-001", None, 100, None).unwrap();
        assert_eq!(violations.len(), 1, "only the layer with delta fires");
        assert!(violations[0].proposed_remediation.contains("a.com"));
    }
}
