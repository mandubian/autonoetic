//! Supply-chain auditing checks (Phase 4, deterministic) — **FROZEN BASELINE**.
//!
//! ## DO NOT EDIT WITHOUT EXPLICIT OPERATOR ACTION.
//!
//! Frozen snapshot of `super::checks::supply_chain` (issue #153).
//! See `super::baseline::credential` for the full editing-rules rationale.
//!
//! Last frozen at `BASELINE_VERSION = 1.0.0` (issue #153, initial freeze).
//! See `super::BASELINE_VERSION` for the version pin and bump policy.

#![allow(dead_code)]

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

