//! Normalized error fingerprinting (issues #703, #705).
//!
//! Produces a stable 64-bit hash of a tool-result error with volatile
//! identifiers stripped — session/task/workflow ids, UUIDs, timestamps, long
//! hashes, and bare integers all collapse to placeholders. The same root-cause
//! error recurring through different tools or turns therefore maps to a single
//! fingerprint.
//!
//! Two consumers share this:
//! - The LoopGuard **recurring-error detector** (#703): the same fingerprint
//!   surfacing from several distinct tools means the agent is trying different
//!   approaches against one unrecoverable cause.
//! - The wire-format **history compaction** pass (#705): a repeated error
//!   tool-result can be collapsed to a short marker so it is not re-sent in
//!   full on every subsequent round.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

use regex::Regex;

// RFC3339-ish timestamps first (they contain digits the number rule would
// otherwise shred). Then UUIDs, prefixed ids, long hex digests, and finally
// bare integers. Order matters — earlier rules protect their matches from
// later, coarser ones.
static TIMESTAMP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}T[0-9:.+Z-]+").expect("valid timestamp regex"));

static UUID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
        .expect("valid uuid regex")
});

// Prefixed entity ids (`wf-fa47e326`, `session-cc54`, `root-abc/planner`,
// `apr-plan-…-v2`). Multi-char prefixes only, so ordinary prose isn't hit.
static PREFIXED_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:wf|task|session|sess|root|apr|rev|job|node|agent|child)-[A-Za-z0-9][A-Za-z0-9_./:-]*",
    )
    .expect("valid prefixed-id regex")
});

static LONG_HEX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-fA-F]{12,}\b").expect("valid hex regex"));

// No word boundaries: also strips digits fused to a unit (`30s`, `120s`) so
// `timed out after 30s` and `after 120s` share a fingerprint.
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+").expect("valid number regex"));

static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid whitespace regex"));

/// Strip volatile identifiers from an error message and canonicalize it so
/// semantically identical errors normalize to the same string.
pub fn normalize_error_text(s: &str) -> String {
    let s = TIMESTAMP_RE.replace_all(s, "<ts>");
    let s = UUID_RE.replace_all(&s, "<id>");
    let s = PREFIXED_ID_RE.replace_all(&s, "<id>");
    let s = LONG_HEX_RE.replace_all(&s, "<hash>");
    let s = NUMBER_RE.replace_all(&s, "<n>");
    let s = WS_RE.replace_all(&s, " ");
    s.trim().to_lowercase()
}

/// Hash of an already-normalized error string. Uses the same hasher family as
/// the LoopGuard progress fingerprints for consistency.
pub fn hash_normalized(normalized: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

/// Pull the salient error text out of a tool-result JSON, or `None` if the
/// result is not an error. Recognizes:
/// - `ok: false`, or the presence of `error` / `error_type`
/// - `any_failed: true` (e.g. `workflow_wait` surfacing a failed child), whose
///   root cause lives in `failure_summary[].{reason,message,result_summary}`.
///   `result_summary` is included because `workflow_wait` populates it with the
///   child failure text.
pub fn extract_error_text(result_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(result_json).ok()?;
    let obj = value.as_object()?;

    let ok = obj.get("ok").and_then(|v| v.as_bool());
    let has_error = obj.contains_key("error") || obj.contains_key("error_type");
    let any_failed = obj
        .get("any_failed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let escalation = obj
        .get("escalation_required")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if ok != Some(false) && !has_error && !any_failed && !escalation {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = obj.get("error").and_then(|v| v.as_str()) {
        parts.push(s.to_string());
    }
    if let Some(s) = obj.get("error_type").and_then(|v| v.as_str()) {
        parts.push(s.to_string());
    }
    if parts.is_empty() {
        if let Some(s) = obj
            .get("reason")
            .and_then(|v| v.as_str())
            .or_else(|| obj.get("message").and_then(|v| v.as_str()))
        {
            parts.push(s.to_string());
        }
    }
    if let Some(arr) = obj.get("failure_summary").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(s) = item
                .get("reason")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("message").and_then(|v| v.as_str()))
                .or_else(|| item.get("result_summary").and_then(|v| v.as_str()))
                .or_else(|| item.get("error").and_then(|v| v.as_str()))
            {
                parts.push(s.to_string());
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Normalized error fingerprint of a tool result, or `None` when the result
/// carries no error.
pub fn fingerprint_result(result_json: &str) -> Option<u64> {
    let text = extract_error_text(result_json)?;
    Some(hash_normalized(&normalize_error_text(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_error_different_ids_shares_fingerprint() {
        let a = normalize_error_text(
            "workflow wf-fa47e326 was reactivated by the root planner and cannot accept child-session spawns",
        );
        let b = normalize_error_text(
            "workflow wf-99bc10ff was reactivated by the root planner and cannot accept child-session spawns",
        );
        assert_eq!(a, b, "volatile workflow id must be stripped");
        assert_eq!(hash_normalized(&a), hash_normalized(&b));
    }

    #[test]
    fn timestamps_and_numbers_normalized() {
        let a = normalize_error_text("timed out after 30s at 2026-07-01T10:00:00Z");
        let b = normalize_error_text("timed out after 120s at 2026-07-02T18:30:12Z");
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_errors_differ() {
        let a = fingerprint_result(r#"{"ok":false,"error":"disk full"}"#).unwrap();
        let b = fingerprint_result(r#"{"ok":false,"error":"permission denied"}"#).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn non_error_result_has_no_fingerprint() {
        assert!(fingerprint_result(r#"{"ok":true,"stdout":"done"}"#).is_none());
        assert!(fingerprint_result(r#"{"ok":true,"any_failed":false}"#).is_none());
    }

    #[test]
    fn any_failed_uses_failure_summary() {
        let fp = fingerprint_result(
            r#"{"ok":true,"any_failed":true,"failure_summary":[{"reason":"workflow wf-abc123def456 was reactivated and cannot accept child-session spawns"}]}"#,
        );
        assert!(fp.is_some());
        let normalized = normalize_error_text(
            "workflow wf-abc123def456 was reactivated and cannot accept child-session spawns",
        );
        assert_eq!(fp.unwrap(), hash_normalized(&normalized));
    }

    #[test]
    fn uppercase_hex_is_normalized() {
        let a = normalize_error_text("artifact ar-abc123def456 not found");
        let b = normalize_error_text("artifact ar-ABC123DEF456 not found");
        assert_eq!(a, b, "uppercase hex digests must normalize to same fingerprint");
        assert_eq!(hash_normalized(&a), hash_normalized(&b));
    }

    #[test]
    fn any_failed_uses_result_summary() {
        let fp_reason = fingerprint_result(
            r#"{"ok":true,"any_failed":true,"failure_summary":[{"reason":"workflow wf-abc123def456 was reactivated"}]}"#,
        );
        let fp_result_summary = fingerprint_result(
            r#"{"ok":true,"any_failed":true,"failure_summary":[{"result_summary":"workflow wf-abc123def456 was reactivated"}]}"#,
        );
        assert_eq!(
            fp_reason, fp_result_summary,
            "result_summary must fingerprint the same as reason"
        );
    }

    #[test]
    fn error_type_only_result_is_fingerprinted() {
        assert!(fingerprint_result(r#"{"ok":false,"error_type":"validation"}"#).is_some());
    }

    #[test]
    fn escalation_result_is_fingerprinted() {
        let json = r#"{"escalation_type":"human","message":"Escalation logged.","reason":"coder.default cannot be spawned","escalation_required":true,"request_id":"esc-0671cf0d"}"#;
        assert!(fingerprint_result(json).is_some(), "escalation_required result must be fingerprinted");
    }

    #[test]
    fn escalation_uses_reason_not_message() {
        // The session.escalate tool always sets a static `message`. The
        // fingerprint must use `reason` (the escalation-specific text) so
        // that different escalation reasons produce different fingerprints.
        let a = r#"{"escalation_required":true,"message":"Escalation logged.","reason":"coder.default cannot be spawned"}"#;
        let b = r#"{"escalation_required":true,"message":"Escalation logged.","reason":"need network approval for api.example.com"}"#;
        let fp_a = fingerprint_result(a).expect("a fingerprinted");
        let fp_b = fingerprint_result(b).expect("b fingerprinted");
        assert_ne!(fp_a, fp_b, "different escalation reasons must produce different fingerprints");
    }

    #[test]
    fn same_escalation_reason_shares_fingerprint_across_request_ids() {
        let a = r#"{"escalation_required":true,"reason":"coder.default cannot be spawned — 4 consecutive failures","request_id":"esc-aaa111"}"#;
        let b = r#"{"escalation_required":true,"reason":"coder.default cannot be spawned — 7 consecutive failures","request_id":"esc-bbb222"}"#;
        let fp_a = fingerprint_result(a).expect("a fingerprinted");
        let fp_b = fingerprint_result(b).expect("b fingerprinted");
        assert_eq!(fp_a, fp_b, "same root-cause escalation (differing only in count/id) must share a fingerprint");
    }

    #[test]
    fn escalation_required_false_not_fingerprinted() {
        let json = r#"{"escalation_required":false,"reason":"something"}"#;
        assert!(fingerprint_result(json).is_none(), "escalation_required:false must not be fingerprinted");
    }
}
