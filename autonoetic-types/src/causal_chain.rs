//! Causal Chain log entry — immutable hash-chain audit trail.

use serde::{Deserialize, Serialize};

/// Status of a causal chain entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntryStatus {
    Success,
    Denied,
    Error,
}

impl std::fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => write!(f, "SUCCESS"),
            Self::Denied => write!(f, "DENIED"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// A single entry in the append-only `.jsonl` Causal Chain log.
///
/// **Principal identity**: `actor_id` *is* the principal identity — it is
/// bound into the entry hash alongside `session_id`, `turn_id`, and
/// `event_seq`. No `agent_id` → `principal_id` rename is planned; the
/// generic identity already lives at the ledger layer. See
/// [`crate::principal`] for the typed principal model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalChainEntry {
    pub timestamp: String,
    pub log_id: String,
    /// Principal identity — bound into the entry hash. See module doc.
    pub actor_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub event_seq: u64,
    pub category: String,
    pub action: String,
    pub target: Option<String>,
    pub status: EntryStatus,
    pub reason: Option<String>,
    pub payload: Option<serde_json::Value>,
    #[serde(default)]
    pub payload_hash: Option<String>,
    pub prev_hash: String,
    #[serde(default)]
    pub entry_hash: String,
}

pub const RULE_ID_EVENT_ATTRIBUTION: &str = "R+++3";

pub fn default_enforced_rules() -> Vec<String> {
    vec![RULE_ID_EVENT_ATTRIBUTION.to_string()]
}

/// Causal event record for storage in gateway.db causal_events table.
/// Matches the schema for queryable event storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalEventRecord {
    pub event_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub event_seq: u64,
    pub timestamp: String,
    pub category: String,
    pub action: String,
    pub status: String,
    #[serde(default = "default_enforced_rules")]
    pub enforced_rules: Vec<String>,
    pub target: Option<String>,
    pub payload: Option<String>,
    pub payload_ref: Option<String>,
    pub evidence_ref: Option<String>,
    pub reason: Option<String>,
}

/// Whether this causal row should drive policy-decision notifications (hooks, chat TUI policy pane).
///
/// Semantics: `DENIED` / `ERROR` always; `SUCCESS` only when any enforced rule is not the baseline
/// attribution rule ([`RULE_ID_EVENT_ATTRIBUTION`]).
pub fn causal_event_notifies_policy_decision(event: &CausalEventRecord) -> bool {
    let s = event.status.as_str();
    if s.eq_ignore_ascii_case("DENIED") || s.eq_ignore_ascii_case("ERROR") {
        return true;
    }
    let has_non_baseline_rule = event
        .enforced_rules
        .iter()
        .any(|r| r.as_str() != RULE_ID_EVENT_ATTRIBUTION);
    if s.eq_ignore_ascii_case("SUCCESS") {
        return has_non_baseline_rule;
    }
    if s.eq_ignore_ascii_case("active") {
        return has_non_baseline_rule;
    }
    false
}

/// Execution trace record for storage in gateway.db execution_traces table.
/// Stores structured tool execution results for agent learning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTraceRecord {
    pub trace_id: String,
    pub event_id: Option<String>,
    pub agent_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub tool_name: String,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub duration_ms: i64,
    pub success: i32,
    pub error_type: Option<String>,
    pub error_summary: Option<String>,
    pub approval_required: Option<i32>,
    pub approval_request_id: Option<String>,
    pub arguments: Option<String>,
    pub result: Option<String>,
    /// Where stdout/stderr/result may flow (RFC data-envelopes §6). `None` =
    /// legacy unlabeled row; the gateway applies `egress.legacy_unlabeled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_label: Option<crate::egress::EgressLabel>,
    /// Host paths this execution could see, as asserted by the gateway when it
    /// built the sandbox (#1002 slice 1): `"ro:"`/`"rw:"`-prefixed entries for
    /// the workspace, session-content, artifact, layer and SDK mounts, plus the
    /// blanket `"ro:host_root"` marker while the legacy whole-host bind is in
    /// effect. `None` for pre-v81 rows and tools that run no sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_set: Option<Vec<String>>,
}

impl CausalEventRecord {
    pub fn redact_for_viewer(&self, viewer: super::disclosure::ViewerClass) -> Self {
        match viewer {
            super::disclosure::ViewerClass::Admin => self.clone(),
            super::disclosure::ViewerClass::Operator => {
                let mut out = self.clone();
                if let Some(ref payload) = self.payload {
                    out.payload = Some(redact_json_string(payload));
                }
                out.payload_ref = None;
                out
            }
            // A decider reads events to judge them; `redact_json_string` is the
            // same in-place masking the operator path uses, so the payload
            // stays legible without carrying secrets into a recorded context.
            super::disclosure::ViewerClass::Decider => {
                let mut out = self.clone();
                if let Some(ref payload) = self.payload {
                    out.payload = Some(redact_json_string(payload));
                }
                out.payload_ref = None;
                out
            }
            super::disclosure::ViewerClass::Agent => Self {
                event_id: self.event_id.clone(),
                agent_id: self.agent_id.clone(),
                session_id: self.session_id.clone(),
                turn_id: self.turn_id.clone(),
                event_seq: self.event_seq,
                timestamp: self.timestamp.clone(),
                category: self.category.clone(),
                action: self.action.clone(),
                status: self.status.clone(),
                enforced_rules: self.enforced_rules.clone(),
                target: self.target.clone(),
                payload: None,
                payload_ref: None,
                evidence_ref: None,
                reason: None,
            },
        }
    }
}

// Redaction primitives are centralised in `crate::redaction`. This local
// thin wrapper is the only call site needed in this module; the previous
// inline copy used a wholesale-redaction fallback that nuked benign strings
// like "tokenizer config" — see issue #156.

fn redact_json_string(s: &str) -> String {
    crate::redaction::redact_text_for_logs(s)
}

/// Session transcript record for storage in gateway.db session_transcripts table.
/// Used for full-text search across conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTranscriptRecord {
    pub transcript_id: String,
    pub session_id: String,
    pub root_session_id: String,
    pub agent_id: String,
    pub revision_id: Option<String>,
    pub user_id: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub turn_count: i64,
    pub transcript_handle: Option<String>,
    pub excerpt: Option<String>,
    pub origin_node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedSessionReportRecord {
    pub root_session_id: String,
    pub report_handle: String,
    pub overview_handle: Option<String>,
    pub html_handle: Option<String>,
    pub narrative_handle: Option<String>,
    pub title: String,
    pub status: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub agent_count: i32,
    pub error_count: i32,
    pub approval_count: i32,
    pub search_text: String,
    pub generated_at: String,
    pub report_version: i32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests for `redact_for_viewer` on CausalEventRecord.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod redaction_tests {
    use super::*;
    use crate::disclosure::ViewerClass;

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Substrings that must never appear in any field of an `Agent`-class
    /// redaction output. Used by property-style assertions.
    const SECRET_TOKENS: &[&str] = &[
        "Bearer eyJhbGc",
        "sk-test-12345",
        "sk-ant-secret",
        "AKIAIOSFODNN",
        "ghp_realtoken",
        "-----BEGIN PRIVATE KEY-----",
        "PASSWORD=hunter2",
        "api_key=verysecret",
    ];

    /// A causal-event fixture stuffed with every secret-bearing string we care about.
    fn event_with_secrets() -> CausalEventRecord {
        CausalEventRecord {
            event_id: "evt_001".into(),
            agent_id: "coder.default".into(),
            session_id: "sess_001".into(),
            turn_id: Some("turn_001".into()),
            event_seq: 7,
            timestamp: "2026-05-08T12:00:00Z".into(),
            category: "tool".into(),
            action: "sandbox_exec".into(),
            status: "SUCCESS".into(),
            enforced_rules: vec!["R+++3".into()],
            target: Some("api.github.com".into()),
            payload: Some(
                r#"{"authorization":"Bearer eyJhbGc.foo.bar","user_id":42}"#.into(),
            ),
            payload_ref: Some("artifact_handle_xyz".into()),
            evidence_ref: Some("evidence_xyz".into()),
            reason: Some("contains AKIAIOSFODNN1234567 in error".into()),
        }
    }

    /// Stringify a record's redacted form into a single blob for property
    /// assertions — any secret leaking into any field will show up here.
    fn event_blob_for(record: &CausalEventRecord, viewer: ViewerClass) -> String {
        let r = record.redact_for_viewer(viewer);
        serde_json::to_string(&r).unwrap_or_default()
    }

    // ── CausalEventRecord ────────────────────────────────────────────────

    #[test]
    fn event_admin_viewer_round_trips() {
        let original = event_with_secrets();
        let redacted = original.redact_for_viewer(ViewerClass::Admin);
        assert_eq!(redacted.payload, original.payload);
        assert_eq!(redacted.payload_ref, original.payload_ref);
        assert_eq!(redacted.evidence_ref, original.evidence_ref);
        assert_eq!(redacted.reason, original.reason);
    }

    #[test]
    fn event_agent_viewer_blanks_payload_and_refs() {
        let r = event_with_secrets().redact_for_viewer(ViewerClass::Agent);
        assert_eq!(r.payload, None);
        assert_eq!(r.payload_ref, None);
        assert_eq!(r.evidence_ref, None);
        assert_eq!(r.reason, None);
    }

    #[test]
    fn event_agent_viewer_preserves_attribution_and_action() {
        let original = event_with_secrets();
        let r = original.redact_for_viewer(ViewerClass::Agent);
        // Attribution and action fields must remain so agents can correlate
        // their own causal chain; redaction strips only the contents.
        assert_eq!(r.event_id, original.event_id);
        assert_eq!(r.agent_id, original.agent_id);
        assert_eq!(r.session_id, original.session_id);
        assert_eq!(r.turn_id, original.turn_id);
        assert_eq!(r.event_seq, original.event_seq);
        assert_eq!(r.timestamp, original.timestamp);
        assert_eq!(r.category, original.category);
        assert_eq!(r.action, original.action);
        assert_eq!(r.status, original.status);
        assert_eq!(r.target, original.target);
        assert_eq!(r.enforced_rules, original.enforced_rules);
    }

    #[test]
    fn event_operator_viewer_redacts_payload_keys_and_clears_payload_ref() {
        // The fix in commit 4ea9df0 (#4): payload_ref must be cleared for
        // non-Admin viewers because resolving it would expose the underlying
        // artifact body. This test pins that contract.
        let r = event_with_secrets().redact_for_viewer(ViewerClass::Operator);
        let payload = r.payload.expect("payload preserved structurally for Operator");
        assert!(
            !payload.contains("Bearer eyJhbGc"),
            "operator must not see authorization value: {payload}"
        );
        assert!(
            payload.contains("user_id"),
            "operator must keep non-secret payload keys: {payload}"
        );
        assert_eq!(
            r.payload_ref, None,
            "payload_ref must be cleared for Operator (issue #4 fix)"
        );
    }

    #[test]
    fn event_agent_viewer_property_no_secrets_leak() {
        let blob = event_blob_for(&event_with_secrets(), ViewerClass::Agent);
        for token in SECRET_TOKENS {
            assert!(
                !blob.contains(token),
                "Agent-class redacted event must not contain '{token}' — full blob: {blob}"
            );
        }
    }

    // ── Direct delegation to canonical helpers ───────────────────────────
    // These tests pin the integration: `causal_chain::redact_for_viewer`
    // now routes string redaction through `crate::redaction`. The canonical
    // module's own unit tests (`crate::redaction::tests`) cover finer-grained
    // behavior; the assertions here verify the wiring stays correct.

    #[test]
    fn redact_json_value_redacts_object_keys_named_like_secrets() {
        let v = serde_json::json!({
            "client_secret": "abc",
            "access_token": "def",
            "refresh_token": "ghi",
            "ok": true,
        });
        let out = crate::redaction::redact_json_value(&v);
        assert_eq!(out["client_secret"], "***REDACTED***");
        assert_eq!(out["access_token"], "***REDACTED***");
        assert_eq!(out["refresh_token"], "***REDACTED***");
        assert_eq!(out["ok"], true);
    }

    #[test]
    fn redact_json_value_handles_each_secret_shape_appropriately() {
        let v = serde_json::json!({
            "header": "Bearer eyJhbGc.tail",
            "key_blob": "-----BEGIN RSA PRIVATE KEY-----\nXXX\n-----END RSA PRIVATE KEY-----",
            "openai_key": "sk-abc123def456ghi789",
            "innocuous": "tokenizer config",
        });
        let out = crate::redaction::redact_json_value(&v);
        // Bearer: in-place masking preserves the prefix, masks the value.
        assert_eq!(out["header"], "Bearer ***REDACTED***");
        // PEM: can't be masked in place; fallback wholesale redact via the
        // narrow `s.contains("-----BEGIN")` branch in `redact_json_value`.
        assert_eq!(out["key_blob"], "***REDACTED***");
        // Bare `sk-…`: handled by `redact_embedded_secrets`'s sk- prefix branch.
        assert_eq!(out["openai_key"], "***REDACTED***");
        // Benign `tokenizer config`: no in-place mask, no secret shape ⇒ kept.
        assert_eq!(out["innocuous"], "tokenizer config");
    }

    #[test]
    fn is_sensitive_key_matches_documented_substrings() {
        for k in &[
            "secret",
            "TOKEN",
            "user_password",
            "api_key",
            "Authorization",
            "AWS_ACCESS_KEY_ID",
            "access_token",
            "refresh_token",
            "client_secret",
        ] {
            assert!(crate::redaction::is_sensitive_key(k), "expected sensitive: {k}");
        }
        for k in &["user_id", "agent_id", "session_id", "items", "ok"] {
            assert!(!crate::redaction::is_sensitive_key(k), "expected non-sensitive: {k}");
        }
    }

    #[test]
    fn is_sensitive_key_misses_hyphenated_api_key() {
        // KNOWN GAP: `X-API-Key` (hyphenated) does NOT match — the substring
        // catalogue uses `api_key` (underscore). Hyphenated `*-Token` does
        // match via the `token` substring. This pin documents the gap.
        assert!(
            !crate::redaction::is_sensitive_key("X-API-Key"),
            "regression: X-API-Key now matches — update is_sensitive_key in \
             autonoetic-types::redaction and this pin together"
        );
        assert!(crate::redaction::is_sensitive_key("X-Auth-Token"));
        assert!(crate::redaction::is_sensitive_key("X-Access-Token"));
    }

    // ── Bug-fix coverage for issue #156 (delegated path) ─────────────────

    #[test]
    fn benign_substrings_do_not_nuke_full_string_via_redact_json_string() {
        // The bug: `redact_json_string("Updated tokenizer config")` used to
        // return "***REDACTED***" wholesale because the string contained the
        // substring "token". After the migration to `redact_text_for_logs`
        // it round-trips.
        for input in &[
            "Updated tokenizer config in v2",
            "secretary-general announcement",
            "the authorization process is documented in section 4",
        ] {
            let out = redact_json_string(input);
            assert_eq!(out, *input, "benign string '{input}' must round-trip");
        }
    }

    #[test]
    fn real_secrets_still_masked_via_redact_json_string() {
        // Bearer token in a non-JSON string — masked in place, prose preserved.
        let out = redact_json_string("Authorization: Bearer eyJhbGc.foo plus context");
        assert!(out.contains("Bearer ***REDACTED***"));
        assert!(out.contains("plus context"));
        assert!(!out.contains("eyJhbGc"));
    }
}
