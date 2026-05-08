//! Sandbox-escape pattern detection.
//!
//! Scans the `sandbox_escape_attempts` table (schema v25) for recorded escape
//! indicators. Also pattern-matches causal event payloads for known-bad command
//! sequences that suggest privilege escalation attempts.

use anyhow::Result;
use autonoetic_types::security::{
    AffectedEntities, EvidenceAnchor, FindingSeverity, FindingType, Reproducibility,
    SecurityFinding,
};
use regex::Regex;
use rusqlite::Connection;
use std::sync::LazyLock;

// ── known-bad command patterns ────────────────────────────────────────────────

/// `nsenter` — enter a running container namespace, a classic escape vector.
static NSENTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bnsenter\b").expect("valid nsenter regex"));

/// `docker run --privileged` or `docker run -v /:/` — mounting host root.
static DOCKER_PRIVILEGED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"docker\s+run\s+.*--privileged|docker\s+run\s+.*-v\s+/:/")
        .expect("valid docker privileged regex")
});

/// `chroot /` with a following command — classic jailbreak pattern.
static CHROOT_ROOT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bchroot\s+/\s+").expect("valid chroot root regex")
});

/// Writing to `/proc/sysrq-trigger` — kernel escape.
static SYSRQ_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/proc/sysrq-trigger").expect("valid sysrq regex")
});

/// `mount --bind /` — bind-mounting host root.
static MOUNT_BIND_ROOT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bmount\s+--bind\s+/").expect("valid mount bind root regex")
});

// ── public API ────────────────────────────────────────────────────────────────

/// Drain new rows from `sandbox_escape_attempts` since `since_rfc3339`.
/// Each row becomes a `critical` `SandboxEscapeAttempt` finding.
///
/// `scope_agent_id` filters to a single agent (used by the pre-promotion gate).
pub fn scan_escape_attempt_records(
    conn: &Connection,
    sentinel_revision_id: &str,
    since_rfc3339: Option<&str>,
    limit: u32,
    scope_agent_id: Option<&str>,
) -> Result<Vec<SecurityFinding>> {
    let since = since_rfc3339.unwrap_or("1970-01-01T00:00:00Z");
    let lim = limit as i64;

    let mut stmt = conn.prepare(
        "SELECT id, session_id, root_session_id, agent_id, indicator, detail, detected_at
         FROM sandbox_escape_attempts
         WHERE detected_at > ?1
           AND (?3 IS NULL OR agent_id = ?3)
         ORDER BY detected_at ASC
         LIMIT ?2",
    )?;

    struct EscapeRow {
        id: i64,
        session_id: String,
        agent_id: String,
        indicator: String,
        detail: Option<String>,
    }

    let rows = stmt
        .query_map(rusqlite::params![since, lim, scope_agent_id], |row| {
            Ok(EscapeRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                agent_id: row.get(3)?,
                indicator: row.get(4)?,
                detail: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    for r in rows {
        let detail = r.detail.as_deref().unwrap_or("(no detail)");
        let finding = SecurityFinding::new(
            FindingType::SandboxEscapeAttempt,
            FindingSeverity::Critical,
            1.0,
            Reproducibility::Deterministic,
            format!(
                "Sandbox escape attempt recorded for agent '{}' in session '{}'. \
                 Indicator: '{}'. Detail: {}. \
                 Investigate immediately and consider revoking the session.",
                r.agent_id, r.session_id, r.indicator, detail
            ),
            sentinel_revision_id,
        )
        .with_affected(AffectedEntities {
            agent_alias: Some(r.agent_id.clone()),
            session_id: Some(r.session_id.clone()),
            ..Default::default()
        })
        .with_anchors(vec![EvidenceAnchor::SandboxEscapeRecord { rowid: r.id }]);
        findings.push(finding);
    }
    Ok(findings)
}

/// Scan recent causal-event payloads for known-bad sandbox-escape command
/// patterns that may have slipped through runtime detection.
///
/// `scope_agent_id` filters to a single agent (used by the pre-promotion gate).
pub fn scan_escape_patterns_in_events(
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

    struct EventRow {
        event_id: String,
        agent_id: String,
        session_id: String,
        payload: String,
    }

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
    for r in rows {
        if let Some(pattern_name) = detect_escape_pattern(&r.payload) {
            let finding = SecurityFinding::new(
                FindingType::SandboxEscapeAttempt,
                FindingSeverity::Critical,
                0.95,
                Reproducibility::Deterministic,
                format!(
                    "Known sandbox-escape command pattern '{}' detected in causal event \
                     payload for agent '{}' session '{}'. \
                     Review the event and the agent's execution history.",
                    pattern_name, r.agent_id, r.session_id
                ),
                sentinel_revision_id,
            )
            .with_affected(AffectedEntities {
                agent_alias: Some(r.agent_id.clone()),
                session_id: Some(r.session_id.clone()),
                ..Default::default()
            })
            .with_anchors(vec![EvidenceAnchor::CausalEvent {
                id: r.event_id.clone(),
            }]);
            findings.push(finding);
        }
    }
    Ok(findings)
}

// ── internals ────────────────────────────────────────────────────────────────

fn detect_escape_pattern(text: &str) -> Option<&'static str> {
    if NSENTER_RE.is_match(text) {
        return Some("nsenter");
    }
    if DOCKER_PRIVILEGED_RE.is_match(text) {
        return Some("docker_privileged_mount");
    }
    if CHROOT_ROOT_RE.is_match(text) {
        return Some("chroot_root");
    }
    if SYSRQ_RE.is_match(text) {
        return Some("sysrq_trigger");
    }
    if MOUNT_BIND_ROOT_RE.is_match(text) {
        return Some("mount_bind_root");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_nsenter() {
        assert_eq!(
            detect_escape_pattern("sudo nsenter -t 1 -m -u -i -n"),
            Some("nsenter")
        );
    }

    #[test]
    fn detects_docker_privileged() {
        assert_eq!(
            detect_escape_pattern("docker run --privileged -it ubuntu bash"),
            Some("docker_privileged_mount")
        );
    }

    #[test]
    fn detects_docker_root_mount() {
        assert_eq!(
            detect_escape_pattern("docker run -v /:/hostroot ubuntu ls /hostroot"),
            Some("docker_privileged_mount")
        );
    }

    #[test]
    fn detects_chroot_root() {
        assert_eq!(
            detect_escape_pattern("chroot / /bin/bash"),
            Some("chroot_root")
        );
    }

    #[test]
    fn detects_sysrq() {
        assert_eq!(
            detect_escape_pattern("echo b > /proc/sysrq-trigger"),
            Some("sysrq_trigger")
        );
    }

    #[test]
    fn no_false_positive_on_normal_commands() {
        assert_eq!(detect_escape_pattern("ls -la /tmp && cargo build"), None);
    }
}
