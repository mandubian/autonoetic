//! Credential-pattern regex scan over causal-event payloads.
//!
//! Reuses the pattern vocabulary from `log_redaction` to detect credentials
//! that were *not* redacted before being logged (indicating a redaction miss or
//! a raw value flowing through a non-redacted path).

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

/// Generic high-entropy hex string (≥ 32 hex chars) — potential raw secret.
static HIGH_ENTROPY_HEX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[a-fA-F0-9]{32,}\b").expect("valid high-entropy hex regex")
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
/// `since_event_id` allows incremental sweeps — pass the last checked event ID
/// to avoid re-scanning the full history. Pass `None` for a full sweep.
pub fn scan_credential_leaks(
    conn: &Connection,
    sentinel_revision_id: &str,
    since_rfc3339: Option<&str>,
    limit: u32,
) -> Result<Vec<SecurityFinding>> {
    let since = since_rfc3339.unwrap_or("1970-01-01T00:00:00Z");
    let lim = limit as i64;

    let mut stmt = conn.prepare(
        "SELECT event_id, agent_id, session_id, payload
         FROM causal_events
         WHERE payload IS NOT NULL
           AND timestamp > ?1
         ORDER BY timestamp ASC
         LIMIT ?2",
    )?;

    let rows = stmt
        .query_map(rusqlite::params![since, lim], |row| {
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
        if let Some((pattern_name, matched)) = detect_credential(&row.payload) {
            let finding = SecurityFinding::new(
                FindingType::CredentialLeak,
                FindingSeverity::Critical,
                1.0,
                Reproducibility::Deterministic,
                format!(
                    "Credential pattern '{}' matched in causal event payload. \
                     Rotate any matching credential immediately. \
                     Matched text (redacted here): {}",
                    pattern_name,
                    redact_match(&matched)
                ),
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

fn detect_credential(text: &str) -> Option<(&'static str, String)> {
    if let Some(m) = ANTHROPIC_KEY_RE.find(text) {
        return Some(("anthropic_api_key", m.as_str().to_string()));
    }
    if let Some(m) = OPENAI_KEY_RE.find(text) {
        return Some(("openai_api_key", m.as_str().to_string()));
    }
    if let Some(m) = AWS_ACCESS_KEY_RE.find(text) {
        return Some(("aws_access_key", m.as_str().to_string()));
    }
    if let Some(m) = GITHUB_PAT_RE.find(text) {
        return Some(("github_pat", m.as_str().to_string()));
    }
    if let Some(m) = BEARER_RE.find(text) {
        return Some(("bearer_token", m.as_str().to_string()));
    }
    if let Some(m) = HIGH_ENTROPY_HEX_RE.find(text) {
        // Only flag hex strings that look like they're in a value position.
        let s = m.as_str();
        if s.len() >= 40 {
            return Some(("high_entropy_hex", s.to_string()));
        }
    }
    None
}

fn redact_match(s: &str) -> String {
    if s.len() <= 8 {
        return "***".to_string();
    }
    format!("{}***", &s[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_openai_key() {
        let text = r#"{"token":"sk-abc12345678901234567890"}"#;
        let result = detect_credential(text);
        assert!(result.is_some());
        let (name, _) = result.unwrap();
        assert_eq!(name, "openai_api_key");
    }

    #[test]
    fn detects_aws_access_key() {
        let text = "AKIAIOSFODNN7EXAMPLE is an AWS key";
        let result = detect_credential(text);
        assert!(result.is_some());
        let (name, _) = result.unwrap();
        assert_eq!(name, "aws_access_key");
    }

    #[test]
    fn detects_anthropic_key() {
        let text = "Using sk-ant-api03-validkeyhere1234567890";
        let result = detect_credential(text);
        assert!(result.is_some());
        let (name, _) = result.unwrap();
        assert_eq!(name, "anthropic_api_key");
    }

    #[test]
    fn detects_github_pat() {
        let text = "token: ghp_1234567890abcdefghijklmnopqrstuvwxyz";
        let result = detect_credential(text);
        assert!(result.is_some());
        let (name, _) = result.unwrap();
        assert_eq!(name, "github_pat");
    }

    #[test]
    fn detects_bearer_token() {
        let text = "Authorization: Bearer eyJhbGciOiJSUzI1NiJ9abcdefgh";
        let result = detect_credential(text);
        assert!(result.is_some());
        let (name, _) = result.unwrap();
        assert_eq!(name, "bearer_token");
    }

    #[test]
    fn no_false_positive_on_short_hex() {
        let text = "hash: deadbeef";
        let result = detect_credential(text);
        assert!(result.is_none(), "short hex should not be flagged");
    }
}
