//! Supply-chain auditing checks (Phase 4, deterministic).
//!
//! ## Check 1 — Layer scope violations
//!
//! Scans `approvals` for granted `layer_mount` actions that carry a non-empty
//! `unapproved_delta`. Each delta entry means the layer was built with network
//! access to hosts that were not covered by the mounting session's grants at
//! approval time. The operator explicitly approved the expansion, so this is
//! not a block — it is an auditable finding.
//!
//! Severity: `critical` when the layer originates from `runtime.lock` (embedded
//! in the agent definition); `warning` for artifact layers.
//!
//! ## Check 2 — Layer provenance gaps
//!
//! For each layer referenced by a granted `layer_mount` approval, checks
//! whether any successful `sandbox_exec` causal event references that
//! `layer_id`. If no capture trace exists in this gateway's causal chain,
//! the operator approved supply-chain content that is not auditable here.
//!
//! Both checks are `Reproducibility::Deterministic` — they query recorded facts.

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
    layer_id: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    mount_path: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    build_time_approved_hosts: Vec<String>,
    #[serde(default)]
    unapproved_delta: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LayerMountPayload {
    #[serde(default)]
    layers: Vec<LayerApprovalEntry>,
}

// ── Check 1: scope violations ─────────────────────────────────────────────────

/// Scan granted `layer_mount` approvals for layers whose build-time scope was
/// not subsumed by the mounting session's grants. Each approved scope expansion
/// is audited as a `SupplyChainScopeViolation` finding.
///
/// Severity:
/// - `critical` when `source = "runtime.lock"` (scope embedded in agent definition)
/// - `warning` for artifact layers (scope was explicitly approved for a single mount)
pub fn scan_layer_scope_violations(
    conn: &Connection,
    sentinel_revision_id: &str,
    since: Option<&str>,
    scan_limit: u32,
) -> Result<Vec<SecurityFinding>> {
    let mut stmt = conn.prepare(
        "SELECT request_id, agent_id, session_id, root_session_id, action_payload, decided_at
         FROM approvals
         WHERE action_type = 'layer_mount'
           AND status IN ('granted', 'auto_granted')
           AND (?1 IS NULL OR decided_at >= ?1)
         ORDER BY decided_at DESC
         LIMIT ?2",
    )?;

    let rows: Vec<(String, String, String, Option<String>, String, Option<String>)> = stmt
        .query_map(params![since, scan_limit as i64], |row| {
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

    for (request_id, agent_id, session_id, root_session_id, action_payload, decided_at) in rows {
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
                    "Layer '{}' (runtime.lock, digest {}) was built with {} host(s) [{}] not in session scope. \
                     Review SKILL.md runtime.lock layers and rebuild with a narrower network scope.",
                    entry.name,
                    &entry.digest[..entry.digest.len().min(16)],
                    entry.unapproved_delta.len(),
                    entry.unapproved_delta.join(", ")
                )
            } else {
                format!(
                    "Layer '{}' (artifact, digest {}) was built with {} host(s) [{}] not in session scope. \
                     Review whether those hosts are still required and re-capture the layer with a narrower scope.",
                    entry.name,
                    &entry.digest[..entry.digest.len().min(16)],
                    entry.unapproved_delta.len(),
                    entry.unapproved_delta.join(", ")
                )
            };

            let mut anchors = vec![EvidenceAnchor::LayerDigest {
                value: entry.digest.clone(),
            }];
            // Record the approval record ID so the finding links back to the DB row.
            anchors.push(EvidenceAnchor::ArtifactId {
                id: request_id.clone(),
            });

            let effective_session = root_session_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&session_id);

            let mut finding = SecurityFinding::new(
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
            .with_anchors(anchors);

            // Attach the approval timestamp as additional context in the finding payload.
            if let Some(ref ts) = decided_at {
                let _ = ts; // available for future structured payload fields
            }

            findings.push(finding);
        }
    }

    Ok(findings)
}

// ── Check 2: provenance gaps ──────────────────────────────────────────────────

/// Scan granted `layer_mount` approvals for layers that have no capture trace
/// in the causal-event store (no successful `sandbox_exec` event referencing
/// the `layer_id` in its `target` or `payload`).
///
/// A gap means: the operator approved mounting supply-chain content that this
/// gateway cannot trace back to a known capture session. This may be benign
/// (layer built on another gateway instance) but warrants operator review.
pub fn scan_layer_provenance_gaps(
    conn: &Connection,
    sentinel_revision_id: &str,
    since: Option<&str>,
    scan_limit: u32,
) -> Result<Vec<SecurityFinding>> {
    // Extract distinct (layer_id, digest, request_id, agent_id, session_id) tuples
    // from granted layer_mount approvals, then filter to those with no capture trace.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT
             json_extract(l.value, '$.layer_id') AS layer_id,
             json_extract(l.value, '$.digest')   AS digest,
             a.request_id, a.agent_id, a.session_id, a.root_session_id
         FROM approvals a, json_each(json_extract(a.action_payload, '$.layers')) AS l
         WHERE a.action_type = 'layer_mount'
           AND a.status IN ('granted', 'auto_granted')
           AND (?1 IS NULL OR a.decided_at >= ?1)
           AND json_extract(l.value, '$.layer_id') IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM causal_events ce
               WHERE ce.action = 'sandbox_exec'
                 AND ce.status = 'success'
                 AND (ce.target = json_extract(l.value, '$.layer_id')
                      OR ce.payload LIKE '%' || json_extract(l.value, '$.layer_id') || '%')
           )
         LIMIT ?2",
    )?;

    let rows: Vec<(String, String, String, String, String, Option<String>)> = stmt
        .query_map(params![since, scan_limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();

    for (layer_id, digest, request_id, agent_id, session_id, root_session_id) in rows {
        let effective_session = root_session_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&session_id);

        let remediation = format!(
            "Layer '{}' (digest {}) was approved for mounting but has no capture trace in \
             this gateway's causal chain. Verify the layer was built under known conditions \
             and consider re-capturing it with full provenance.",
            layer_id,
            &digest[..digest.len().min(16)],
        );

        let finding = SecurityFinding::new(
            FindingType::SupplyChainScopeViolation,
            FindingSeverity::Warning,
            0.8,
            Reproducibility::Deterministic,
            remediation,
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(agent_id.clone()),
            session_id: Some(effective_session.to_string()),
            layer_digest: Some(digest.clone()),
            ..Default::default()
        })
        .with_anchors(vec![
            EvidenceAnchor::LayerDigest { value: digest },
            EvidenceAnchor::ArtifactId { id: request_id },
        ]);

        findings.push(finding);
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
            CREATE TABLE causal_events (
                event_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                turn_id TEXT,
                event_seq INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                category TEXT NOT NULL,
                action TEXT NOT NULL,
                status TEXT NOT NULL,
                enforced_rules TEXT NOT NULL DEFAULT '[]',
                target TEXT,
                payload TEXT,
                payload_ref TEXT,
                evidence_ref TEXT,
                reason TEXT
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
            "INSERT INTO approvals (request_id, agent_id, session_id, action_type, action_payload, status, created_at, decided_at, approval_level)
             VALUES (?1, ?2, 'sess_001', 'layer_mount', ?3, ?4, '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z', 'operator')",
            params![request_id, agent_id, payload, status],
        )
        .unwrap();
    }

    #[test]
    fn scope_violation_flagged_for_unapproved_delta() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn,
            "apr-001",
            "coder.default",
            "granted",
            r#"[{"layer_id":"layer_abc","digest":"sha256:aabbcc","name":"python-deps","mount_path":"/deps","source":"artifact:art_001","build_time_approved_hosts":["pypi.org"],"unapproved_delta":["pypi.org"]}]"#,
        );

        let findings =
            scan_layer_scope_violations(&conn, "rev-001", None, 100).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert_eq!(findings[0].finding_type, FindingType::SupplyChainScopeViolation);
    }

    #[test]
    fn scope_violation_critical_for_runtime_lock_source() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn,
            "apr-002",
            "coder.default",
            "granted",
            r#"[{"layer_id":"layer_def","digest":"sha256:ddeeff","name":"locked-deps","mount_path":"/deps","source":"runtime.lock","build_time_approved_hosts":["private.registry.internal"],"unapproved_delta":["private.registry.internal"]}]"#,
        );

        let findings =
            scan_layer_scope_violations(&conn, "rev-001", None, 100).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Critical);
    }

    #[test]
    fn scope_violation_skipped_when_delta_empty() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn,
            "apr-003",
            "coder.default",
            "granted",
            r#"[{"layer_id":"layer_clean","digest":"sha256:112233","name":"cached-deps","mount_path":"/deps","source":"artifact:art_002","build_time_approved_hosts":["pypi.org"],"unapproved_delta":[]}]"#,
        );

        let findings =
            scan_layer_scope_violations(&conn, "rev-001", None, 100).unwrap();
        assert!(findings.is_empty(), "empty delta must not produce a finding");
    }

    #[test]
    fn scope_violation_skipped_for_non_granted_status() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn,
            "apr-004",
            "coder.default",
            "pending", // not yet decided
            r#"[{"layer_id":"layer_pend","digest":"sha256:445566","name":"py","mount_path":"/deps","source":"artifact:x","build_time_approved_hosts":["pypi.org"],"unapproved_delta":["pypi.org"]}]"#,
        );

        let findings =
            scan_layer_scope_violations(&conn, "rev-001", None, 100).unwrap();
        assert!(findings.is_empty(), "pending approval must not fire");
    }

    #[test]
    fn provenance_gap_flagged_when_no_capture_trace() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn,
            "apr-005",
            "coder.default",
            "granted",
            r#"[{"layer_id":"layer_gap","digest":"sha256:778899","name":"unknown-origin","mount_path":"/deps","source":"artifact:art_003","build_time_approved_hosts":[],"unapproved_delta":[]}]"#,
        );

        let findings = scan_layer_provenance_gaps(&conn, "rev-001", None, 100).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].finding_type, FindingType::SupplyChainScopeViolation);
    }

    #[test]
    fn provenance_gap_not_flagged_when_capture_trace_exists() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn,
            "apr-006",
            "coder.default",
            "granted",
            r#"[{"layer_id":"layer_known","digest":"sha256:aabbdd","name":"known-origin","mount_path":"/deps","source":"artifact:art_004","build_time_approved_hosts":[],"unapproved_delta":[]}]"#,
        );
        // Insert a capture trace event referencing the layer_id.
        conn.execute(
            "INSERT INTO causal_events (event_id, agent_id, session_id, event_seq, timestamp, category, action, status, enforced_rules, target)
             VALUES ('evt_cap_001', 'packager.default', 'sess_build_001', 0, '2026-01-01T00:00:00Z', 'tool', 'sandbox_exec', 'success', '[]', 'layer_known')",
            [],
        )
        .unwrap();

        let findings = scan_layer_provenance_gaps(&conn, "rev-001", None, 100).unwrap();
        assert!(findings.is_empty(), "layer with capture trace must not produce gap finding");
    }

    #[test]
    fn multiple_layers_in_one_approval_each_checked() {
        let conn = open_db();
        insert_layer_mount_approval(
            &conn,
            "apr-007",
            "coder.default",
            "granted",
            r#"[
                {"layer_id":"layer_a1","digest":"sha256:111111","name":"a","mount_path":"/a","source":"artifact:x","build_time_approved_hosts":["a.com"],"unapproved_delta":["a.com"]},
                {"layer_id":"layer_b2","digest":"sha256:222222","name":"b","mount_path":"/b","source":"artifact:y","build_time_approved_hosts":["b.com"],"unapproved_delta":[]}
            ]"#,
        );

        let violations =
            scan_layer_scope_violations(&conn, "rev-001", None, 100).unwrap();
        assert_eq!(violations.len(), 1, "only the layer with delta fires");
        assert!(violations[0].proposed_remediation.contains("a.com"));
    }
}
