//! Credential-pattern regex scan over causal-event payloads — **FROZEN BASELINE**.
//!
//! ## DO NOT EDIT WITHOUT EXPLICIT OPERATOR ACTION.
//!
//! This file is a frozen snapshot of `super::checks::credential` (issue #153).
//! Its purpose is to provide an independent reference for the dual-sweep:
//! a regex regression in `super::checks::credential` will not propagate here,
//! so the baseline pass will continue to flag the regressed-out anchor and
//! `dual_sweep::compare_phase1` will record a `baseline_only` disagreement,
//! catching the regression at the next sweep.
//!
//! **Editing rules:**
//!
//! - The default expectation is that this file is *never* edited. Improvements
//!   to credential detection go to `super::checks::credential` so the canonical
//!   sentinel benefits, while this baseline keeps a stable reference.
//! - The frozen baseline is updated only when the operator deliberately wants
//!   the *baseline* to evolve (e.g. retiring an obsolete credential format).
//!   Such commits should carry a `[baseline-update]` prefix in the commit
//!   message and be reviewed as a separate PR from any `super::checks::*`
//!   change. PRs that touch both `checks/` and `baseline/` for the same
//!   pattern defeat the purpose of having a baseline.
//!
//! Last frozen at `BASELINE_VERSION = 1.0.0` (issue #153, initial freeze).
//! Pattern vocabulary mirrors `super::checks::credential` at the time this
//! file was created. See `super::BASELINE_VERSION` for the version pin and
//! the bump policy.

#![allow(dead_code)]

use anyhow::Result;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding,
};
use regex::Regex;
use rusqlite::Connection;
use std::sync::LazyLock;

// ── credential pattern vocabulary ────────────────────────────────────────────

/// AWS access key: AKIA… (20 uppercase alphanumeric chars)
static AWS_ACCESS_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bAKIA[A-Z0-9]{16}\b").expect("valid aws access key regex")
});

/// OpenAI API key: sk-…
static OPENAI_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bsk-[A-Za-z0-9]{20,}\b").expect("valid openai key regex")
});

/// GitHub personal access token: ghp_…, gho_…, ghs_…, ghr_…
static GITHUB_PAT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bgh[poshpr]_[A-Za-z0-9]{36}\b").expect("valid github pat regex")
});

/// Generic high-entropy hex string (≥ 64 hex chars = 256 bits) — potential raw secret.
///
/// The lower bound is 64 specifically to exclude git SHAs (40 hex chars) and other
/// 40-char content digests that are routinely embedded in causal-event payloads.
/// 64+ hex characters indicate sha256 / 256-bit symmetric keys / similar — the
/// regime where false positives drop sharply.
static HIGH_ENTROPY_HEX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[a-fA-F0-9]{64,}\b").expect("valid high-entropy hex regex")
});

/// Bearer token still present in payload.
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9\-._~+/]{16,}").expect("valid bearer regex")
});

/// Anthropic API key: sk-ant-…
static ANTHROPIC_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bsk-ant-[A-Za-z0-9\-]{20,}\b").expect("valid anthropic key regex")
});

// ── row type from the query ───────────────────────────────────────────────────

struct EventRow {
    event_id: String,
    agent_id: String,
    session_id: String,
    payload: String,
}

// ── public API ────────────────────────────────────────────────────────────────

/// Scan recent causal-event payloads for credential-pattern matches.
///
/// `since_rfc3339` allows incremental sweeps — pass an RFC-3339 timestamp to
/// scan only events after that point. Pass `None` for a full history sweep.
/// `scope_agent_id` filters to events attributed to a specific agent (used
/// by the pre-promotion gate so a leak in agent A's history does not block
/// promotion of agent B). `None` = no filter.
pub fn scan_credential_leaks(
    conn: &Connection,
    sentinel_revision_id: &str,
    since_rfc3339: Option<&str>,
    limit: u32,
    scope_agent_id: Option<&str>,
) -> Result<Vec<SecurityFinding>> {
    let since = since_rfc3339.unwrap_or("1970-01-01T00:00:00Z");
    let lim = limit as i64;

    let mut stmt = conn.prepare(
        "SELECT event_id, agent_id, session_id, payload
         FROM causal_events
         WHERE payload IS NOT NULL
           AND timestamp > ?1
           AND (?3 IS NULL OR agent_id = ?3)
         ORDER BY timestamp ASC
         LIMIT ?2",
    )?;

    let rows = stmt
        .query_map(rusqlite::params![since, lim, scope_agent_id], |row| {
            Ok(EventRow {
                event_id: row.get(0)?,
                agent_id: row.get(1)?,
                session_id: row.get(2)?,
                payload: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for row in rows {
        if let Some((pattern_name, match_len)) = detect_credential(&row.payload) {
            // `high_entropy_hex` is a heuristic — its regex matches any 64+
            // hex string (sha256, AES-256 keys, but also legitimate digests).
            // It lands at Warning so it cannot single-handedly block a
            // promotion via the sentinel gate, and its remediation message
            // calls for verification first rather than immediate rotation.
            // All other patterns (specific key formats, bearer tokens) stay
            // at Critical because the regex shape is highly specific to a
            // known credential format.
            let is_heuristic = pattern_name == "high_entropy_hex";
            let severity = if is_heuristic {
                FindingSeverity::Warning
            } else {
                FindingSeverity::Critical
            };
            let confidence = if is_heuristic { 0.6 } else { 1.0 };
            let remediation = if is_heuristic {
                format!(
                    "Heuristic credential pattern '{}' matched in causal event payload \
                     (match length: {} chars). This regex matches any high-entropy hex \
                     string and may be a legitimate sha256 digest, content hash, or \
                     symmetric key in transit. Investigate via the evidence anchor: \
                     retrieve the event, identify the matched value, and rotate only \
                     if it is a real secret. Mark as false_positive otherwise.",
                    pattern_name, match_len
                )
            } else {
                format!(
                    "Credential pattern '{}' matched in causal event payload \
                     (match length: {} chars). The regex shape is specific to a known \
                     credential format. Rotate any matching credential immediately. \
                     Use the evidence anchor to retrieve the event under controlled access.",
                    pattern_name, match_len
                )
            };
            let finding = SecurityFinding::new(
                FindingType::CredentialLeak,
                severity,
                confidence,
                Reproducibility::Deterministic,
                remediation,
                sentinel_revision_id,
            )
            .with_affected(AffectedEntities {
                agent_alias: Some(row.agent_id.clone()),
                session_id: Some(row.session_id.clone()),
                ..Default::default()
            })
            .with_anchors(vec![EvidenceAnchor::CausalEvent {
                id: row.event_id.clone(),
            }]);
            findings.push(finding);
        }
    }
    Ok(findings)
}

// ── internals ────────────────────────────────────────────────────────────────

/// Returns `(pattern_name, match_length)` — intentionally no matched text to
/// prevent re-introducing credential material into the findings store.
fn detect_credential(text: &str) -> Option<(&'static str, usize)> {
    if let Some(m) = ANTHROPIC_KEY_RE.find(text) {
        return Some(("anthropic_api_key", m.len()));
    }
    if let Some(m) = OPENAI_KEY_RE.find(text) {
        return Some(("openai_api_key", m.len()));
    }
    if let Some(m) = AWS_ACCESS_KEY_RE.find(text) {
        return Some(("aws_access_key", m.len()));
    }
    if let Some(m) = GITHUB_PAT_RE.find(text) {
        return Some(("github_pat", m.len()));
    }
    if let Some(m) = BEARER_RE.find(text) {
        return Some(("bearer_token", m.len()));
    }
    if let Some(m) = HIGH_ENTROPY_HEX_RE.find(text) {
        return Some(("high_entropy_hex", m.len()));
    }
    None
}

