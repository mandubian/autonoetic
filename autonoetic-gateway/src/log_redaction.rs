//! Log redaction helpers to avoid leaking secrets in traces.
//!
//! The [`RedactedPayload`] newtype enforces R+9: redaction happens **before**
//! causal-chain append. Callers must wrap payloads through one of the
//! constructors; raw `serde_json::Value` cannot be passed to
//! [`CausalLogger::log`] or [`CausalLogger::log_durable`].
//!
//! The redaction primitives themselves now live in the workspace's
//! `autonoetic-types::redaction` module, shared with the per-actor
//! viewer redaction in `causal_chain` and `background`. This file
//! re-exports them so existing call sites (`crate::log_redaction::…`)
//! continue to work unchanged.

use serde_json::Value;

pub use autonoetic_types::redaction::{
    looks_like_secret_collection_prompt, looks_like_secret_value, redact_embedded_secrets,
    redact_json_value, redact_text_for_logs, REDACTED,
};

#[derive(Debug, Clone)]
pub struct RedactedPayload(Value);

impl RedactedPayload {
    pub fn from_raw(value: Value) -> Self {
        Self(redact_json_value(&value))
    }

    pub fn from_raw_str(text: &str) -> Self {
        let redacted = redact_text_for_logs(text);
        Self(serde_json::from_str(&redacted).unwrap_or(Value::String(redacted)))
    }

    pub fn from_redacted(value: Value) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> Value {
        self.0
    }

    pub fn as_inner(&self) -> &Value {
        &self.0
    }
}

impl From<Value> for RedactedPayload {
    fn from(value: Value) -> Self {
        Self::from_raw(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The redaction logic itself is tested in `autonoetic-types::redaction::tests`.
    // These tests cover the gateway-side `RedactedPayload` wrapper and pin the
    // public API the rest of the gateway relies on.

    #[test]
    fn redacted_payload_from_raw_redacts_sensitive_keys() {
        let v = serde_json::json!({"token": "abc", "safe": "ok"});
        let payload = RedactedPayload::from_raw(v);
        let inner = payload.as_inner();
        assert_eq!(inner["token"], REDACTED);
        assert_eq!(inner["safe"], "ok");
    }

    #[test]
    fn redacted_payload_from_raw_str_handles_json() {
        let text = r#"{"client_secret":"abc","ok":true}"#;
        let payload = RedactedPayload::from_raw_str(text);
        let inner = payload.as_inner();
        assert_eq!(inner["client_secret"], REDACTED);
        assert_eq!(inner["ok"], true);
    }

    #[test]
    fn redacted_payload_from_raw_str_handles_plain_text() {
        let payload = RedactedPayload::from_raw_str("plain log line, nothing secret");
        match payload.as_inner() {
            Value::String(s) => {
                assert_eq!(s, "plain log line, nothing secret");
            }
            other => panic!("expected string variant, got {other:?}"),
        }
    }

    #[test]
    fn redacted_payload_from_raw_str_masks_embedded_bearer() {
        let payload =
            RedactedPayload::from_raw_str("Authorization: Bearer eyJhbGc.foo plus context");
        match payload.as_inner() {
            Value::String(s) => {
                assert!(s.contains("Bearer ***REDACTED***"));
                assert!(s.contains("plus context"));
                assert!(!s.contains("eyJhbGc"));
            }
            other => panic!("expected string variant, got {other:?}"),
        }
    }

    #[test]
    fn looks_like_secret_value_reexport_works() {
        assert!(looks_like_secret_value("Bearer eyJhbGc.foo"));
        assert!(!looks_like_secret_value("plain"));
    }

    #[test]
    fn looks_like_secret_collection_prompt_reexport_works() {
        assert!(looks_like_secret_collection_prompt("Please paste your API key"));
        assert!(!looks_like_secret_collection_prompt("Hello"));
    }
}
