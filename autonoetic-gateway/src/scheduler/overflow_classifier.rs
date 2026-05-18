//! Overflow error classifier for Phase 3 overflow-aware orchestration.
//!
//! Distinguishes context overflow errors from other API errors so the
//! scheduler can retry exactly once with an aggressive governor pipeline.

/// Whether the error chain contains a `context_overflow:` classification
/// emitted by the LLM drivers or the context governor.
pub fn is_context_overflow(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err);
    msg.contains("context_overflow:")
}

/// Whether the error chain indicates the task already exhausted its
/// overflow retry and should be marked terminal.
pub fn is_terminal_overflow(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err);
    msg.contains("context_overflow_terminal")
}

/// Whether the overflow retry classifier feature flag is enabled.
pub fn overflow_retry_classifier_enabled() -> bool {
    std::env::var("AUTONOETIC_OVERFLOW_RETRY_CLASSIFIER")
        .as_deref()
        == Ok("1")
}
