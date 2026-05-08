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
        if let Some((pattern_name, match_len)) = detect_credential(&row.payload) {
            // `high_entropy_hex` is a heuristic and lands at Warning so it can
            // never single-handedly block a promotion via the sentinel gate.
            // All other patterns (specific key formats, bearer tokens) stay at
            // Critical because the regex shape is highly specific.
            let severity = if pattern_name == "high_entropy_hex" {
                FindingSeverity::Warning
            } else {
                FindingSeverity::Critical
            };
            let confidence = if pattern_name == "high_entropy_hex" { 0.6 } else { 1.0 };
            let finding = SecurityFinding::new(
                FindingType::CredentialLeak,
                severity,
                confidence,
                Reproducibility::Deterministic,
                format!(
                    "Credential pattern '{}' matched in causal event payload \
                     (match length: {} chars). Rotate any matching credential \
                     immediately. Use the evidence anchor to retrieve the event \
                     under controlled access.",
                    pattern_name, match_len
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

    #[test]
    fn no_false_positive_on_git_sha() {
        // Git SHAs are 40 hex chars and appear constantly in causal-event payloads
        // (commit refs, content digests, artifact IDs). They must not be flagged.
        let text = "Updated to commit a1b2c3d4e5f67890123456789012345678901234";
        let result = detect_credential(text);
        assert!(
            result.is_none(),
            "git SHA (40 hex chars) must not match high_entropy_hex"
        );
    }

    #[test]
    fn no_false_positive_on_short_sha256_truncation() {
        // Some tools truncate sha256 to 40, 48, or 56 hex chars in display output.
        // None of those should trigger high_entropy_hex (threshold is 64).
        for len in [40, 48, 56, 63] {
            let hex: String = "a".repeat(len);
            let result = detect_credential(&format!("digest: {}", hex));
            assert!(
                result.is_none(),
                "{}-char hex string must not match high_entropy_hex",
                len
            );
        }
    }

    #[test]
    fn detects_full_sha256_as_high_entropy_hex() {
        // 64 hex chars = sha256, the legitimate detection target.
        let hex: String = "a".repeat(64);
        let text = format!("raw secret: {}", hex);
        let (name, len) = detect_credential(&text).expect("64-char hex must be flagged");
        assert_eq!(name, "high_entropy_hex");
        assert_eq!(len, 64);
    }
}
