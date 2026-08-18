//! Canonical extraction of a JSON payload from an LLM's final reply.
//!
//! An `io.returns` schema asks the agent for a JSON object. What arrives is a
//! *message*, and models decorate messages: DeepSeek and minimax-m3 emit
//! `<think>…</think>` blocks inline in the reply body, most models like to wrap
//! JSON in a markdown fence, and a polite model writes a sentence before the
//! payload ("Here is the credential handoff:") — the exact shape that voided a
//! completed credential ceremony in issue #1104.
//!
//! Every reader of a reply-as-JSON needs the same tolerance, so it lives here
//! once instead of once per call site: the `io.returns` response-validation
//! gate, agent-outcome detection
//! ([`crate::task_completion::extract_agent_outcome`]), and the gateway's
//! self-report claim guards all go through [`extract_reply_json`].
//!
//! The ladder is ordered by how much the reader has to *assume*:
//!
//! 1. [`ReplyJsonSource::Whole`] — the reply is JSON. No assumption.
//! 2. [`ReplyJsonSource::CodeFence`] — the first fenced block that parses.
//! 3. [`ReplyJsonSource::ProseSpan`] — a balanced `{…}`/`[…]` span carved out
//!    of surrounding prose.
//!
//! Each rung only runs when the ones above it failed, and every rung must
//! produce text that actually parses — a span is never *guessed* into being a
//! payload. The returned [`ReplyJsonSource`] tells the caller how far down the
//! ladder it went, so tolerance stays observable (the gateway records rungs 2
//! and 3 as a `P-5.2` normalization through `note_llm_normalization`) rather
//! than silently reshaping the agent's output.

use serde_json::Value;

/// How far down the tolerance ladder [`extract_reply_json`] had to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyJsonSource {
    /// The whole reply (after `<think>` removal) parsed as JSON.
    Whole,
    /// The payload came out of a markdown code fence.
    CodeFence,
    /// The payload was a balanced brace/bracket span inside surrounding prose.
    ProseSpan,
}

impl ReplyJsonSource {
    /// A stable label for tracing/instrumentation.
    pub fn label(self) -> &'static str {
        match self {
            ReplyJsonSource::Whole => "whole_reply",
            ReplyJsonSource::CodeFence => "markdown_code_fence",
            ReplyJsonSource::ProseSpan => "prose_wrapped_json",
        }
    }

    /// A short, redacted description of what was reshaped — suitable for a
    /// normalization trace. Never carries reply content.
    pub fn normalization_detail(self) -> &'static str {
        match self {
            ReplyJsonSource::Whole => "reply parsed as JSON verbatim",
            ReplyJsonSource::CodeFence => "stripped markdown code fences wrapping a JSON reply",
            ReplyJsonSource::ProseSpan => "extracted a JSON payload from surrounding prose",
        }
    }

    /// True when locating the payload required reshaping the reply — i.e. the
    /// gateway extended tolerance and should say so.
    pub fn is_normalization(self) -> bool {
        !matches!(self, ReplyJsonSource::Whole)
    }
}

/// A JSON payload located in a reply, plus how it was located.
#[derive(Debug, Clone)]
pub struct ReplyJson {
    pub value: Value,
    pub source: ReplyJsonSource,
}

/// Strip `<think>…</think>` reasoning blocks that some models (minimax-m3,
/// DeepSeek, Qwen) emit inline in the assistant reply text. Unlike Anthropic's
/// native thinking channel, these are part of the reply payload and leak into
/// validation, history, and display. Stripping them early ensures downstream
/// JSON parsing, schema validation, and chat rendering see clean content.
///
/// Handles both closed (`<think>…</think>`) and unclosed (`<think>…` to end)
/// blocks. Returns the text with all think blocks removed and leading/trailing
/// whitespace trimmed — including when there was nothing to strip, so the same
/// reply does not come back shaped differently depending on whether the model
/// happened to emit a think block. Trimming the no-tag case still borrows, so
/// the fast path allocates nothing.
pub fn strip_think_blocks(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains("<think>") {
        return std::borrow::Cow::Borrowed(s.trim());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 7..];
        if let Some(end) = after_open.find("</think>") {
            rest = &after_open[end + 8..];
        } else {
            // Unclosed think block — discard everything after `<think>`.
            rest = "";
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out.trim().to_string())
}

/// Locate the JSON payload in an LLM reply, tolerating `<think>` blocks,
/// markdown fences, and surrounding prose.
///
/// Returns `None` when no rung of the ladder yields parseable JSON — a reply
/// that is only prose stays only prose, so a real "the agent never produced
/// structured output" violation is never papered over.
pub fn extract_reply_json(reply: &str) -> Option<ReplyJson> {
    let cleaned = strip_think_blocks(reply);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(ReplyJson {
            value,
            source: ReplyJsonSource::Whole,
        });
    }

    if let Some(value) = extract_first_fenced_json(trimmed) {
        return Some(ReplyJson {
            value,
            source: ReplyJsonSource::CodeFence,
        });
    }

    if let Some(value) = extract_prose_wrapped_json(trimmed) {
        return Some(ReplyJson {
            value,
            source: ReplyJsonSource::ProseSpan,
        });
    }

    None
}

/// [`extract_reply_json`] when the caller only wants the payload.
pub fn extract_reply_json_value(reply: &str) -> Option<Value> {
    extract_reply_json(reply).map(|r| r.value)
}

/// The first markdown code fence whose contents parse as JSON.
///
/// Scans fence-by-fence rather than taking the first fence blindly: a reply
/// that shows a shell command in one fence and the payload in the next must
/// still validate.
fn extract_first_fenced_json(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    while pos < len {
        if bytes[pos] == b'`' && pos + 2 < len && &bytes[pos..pos + 3] == b"```" {
            pos += 3;
            // Skip the info string (```json, ```JSON, …) up to end of line.
            while pos < len && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\r' {
                pos += 1;
            }
            if pos < len && bytes[pos] == b'\n' {
                pos += 1;
            }
            let content_start = pos;
            let mut search = content_start;
            while search < len {
                if bytes[search] == b'`' && search + 2 < len && &bytes[search..search + 3] == b"```"
                {
                    let content = s[content_start..search].trim();
                    if let Ok(value) = serde_json::from_str::<Value>(content) {
                        return Some(value);
                    }
                    break;
                }
                search += 1;
            }
            pos = if search + 3 < len { search + 3 } else { len };
        } else {
            pos += 1;
        }
    }
    None
}

/// Maximum number of candidate opening delimiters tried per shape. A reply is
/// already length-bounded by `io.output_policy.max_reply_length_chars`, but a
/// prose-heavy reply can hold many stray braces; a small cap keeps the scan
/// cheap and makes "the payload is buried past the 16th `{`" fail as a
/// violation rather than as a long scan.
const MAX_SPAN_CANDIDATES: usize = 16;

/// Carve a JSON payload out of surrounding prose.
///
/// Three rungs, ordered so the *whole* payload wins over a fragment of it:
///
/// 1. The outermost object span — first `{` to last `}`. That is the payload
///    whenever the prose itself is brace-free, i.e. the ordinary case.
/// 2. The outermost array span — first `[` to last `]`. Objects go first because
///    a prose enumeration (`step [1] then [2]`) parses as the JSON array `[1]`,
///    and accepting that over an object handoff later in the same reply would
///    turn "prose-wrapped" into "wrong type".
/// 3. The *longest* balanced span of either shape. This is what separates the
///    payload from prose carrying its own braces; taking the longest rather than
///    the first keeps an illustrative `{"a": 1}` earlier in the reply from
///    beating the real payload.
fn extract_prose_wrapped_json(s: &str) -> Option<Value> {
    if let Some(value) = outermost_span(s, b'{', b'}') {
        return Some(value);
    }
    if let Some(value) = outermost_span(s, b'[', b']') {
        return Some(value);
    }
    longest_balanced_span(s)
}

/// First `open` to last `close`, parsed if it is JSON.
fn outermost_span(s: &str, open: u8, close: u8) -> Option<Value> {
    let start = s.as_bytes().iter().position(|b| *b == open)?;
    let end = s.as_bytes().iter().rposition(|b| *b == close)?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&s[start..=end]).ok()
}

/// The longest balanced `{…}`/`[…]` span that parses as JSON, ignoring
/// delimiters inside JSON string literals (so `{"note": "}"}` is not cut short
/// at the brace inside the string). Ties go to the earliest span.
fn longest_balanced_span(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    let mut best: Option<(usize, Value)> = None;
    for (open, close) in [(b'{', b'}'), (b'[', b']')] {
        let mut candidates = 0;
        for start in 0..bytes.len() {
            if bytes[start] != open {
                continue;
            }
            candidates += 1;
            if candidates > MAX_SPAN_CANDIDATES {
                break;
            }
            let Some(end) = balanced_end(bytes, start, open, close) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&s[start..=end]) else {
                continue;
            };
            let len = end - start;
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, value));
            }
        }
    }
    best.map(|(_, value)| value)
}

/// Index of the `close` byte that balances the `open` at `start`, tracking JSON
/// string state and escapes. `None` if the span never closes.
fn balanced_end(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(reply: &str) -> Option<(Value, ReplyJsonSource)> {
        extract_reply_json(reply).map(|r| (r.value, r.source))
    }

    #[test]
    fn bare_json_is_whole_and_unnormalized() {
        let (v, source) = extract(r#"{"status":"ok"}"#).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(source, ReplyJsonSource::Whole);
        assert!(!source.is_normalization());
    }

    #[test]
    fn fenced_json_reports_code_fence() {
        let (v, source) = extract("```json\n{\"status\":\"pass\"}\n```").unwrap();
        assert_eq!(v["status"], "pass");
        assert_eq!(source, ReplyJsonSource::CodeFence);
        assert!(source.is_normalization());
    }

    #[test]
    fn fence_scan_skips_a_non_json_fence() {
        let reply = "First I ran:\n```bash\nls -la\n```\nThen:\n```json\n{\"status\":\"ok\"}\n```";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(source, ReplyJsonSource::CodeFence);
    }

    /// The #1104 shape: one sentence of prose, then the JSON handoff, no fence.
    #[test]
    fn leading_prose_then_json_is_recovered() {
        let reply = "The credential ceremony is complete. Here is the handoff:\n\
                     {\"service\":\"github\",\"credential_id\":\"cred_1\",\"ready_for_execution\":true}";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v["credential_id"], "cred_1");
        assert_eq!(source, ReplyJsonSource::ProseSpan);
        assert_eq!(source.label(), "prose_wrapped_json");
    }

    #[test]
    fn trailing_prose_after_json_is_recovered() {
        let reply = "{\"status\":\"ok\"}\n\nLet me know if you need anything else!";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(source, ReplyJsonSource::ProseSpan);
    }

    #[test]
    fn prose_on_both_sides_is_recovered() {
        let reply = "Done. Result:\n{\"status\":\"ok\",\"summary\":\"vaulted\"}\nHappy to help.";
        let (v, _) = extract(reply).unwrap();
        assert_eq!(v["summary"], "vaulted");
    }

    #[test]
    fn braces_inside_string_values_do_not_truncate_the_span() {
        let reply = "Here you go: {\"summary\":\"used ${VAR} and a } brace\",\"status\":\"ok\"}";
        let (v, _) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
        assert!(v["summary"].as_str().unwrap().contains('}'));
    }

    #[test]
    fn escaped_quote_inside_string_does_not_end_the_string() {
        let reply = r#"Result: {"summary":"he said \"} done\"","status":"ok"}"#;
        let (v, _) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
    }

    /// Prose that carries its own braces before the payload: the outermost
    /// first-`{`-to-last-`}` span cannot parse, so the balanced-span rung is
    /// what recovers the real object.
    #[test]
    fn prose_with_its_own_braces_falls_back_to_balanced_span() {
        let reply = "Set {PLACEHOLDER in the template. Handoff: {\"status\":\"ok\"}";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(source, ReplyJsonSource::ProseSpan);
    }

    /// A bracketed enumeration in the prose parses as the JSON array `[1]`.
    /// Objects must win, or a prose-wrapped object handoff would be reported as
    /// "expected object, got array".
    #[test]
    fn object_payload_wins_over_a_bracket_enumeration_in_prose() {
        let reply = "Steps: [1] vault the secret [2] verify.\n{\"status\":\"ok\"}";
        let (v, _) = extract(reply).unwrap();
        assert!(v.is_object(), "got {v}");
        assert_eq!(v["status"], "ok");
    }

    /// An illustrative snippet earlier in the reply must not beat the payload:
    /// the outermost span cannot parse (two objects, prose between), so the
    /// longest balanced span decides.
    #[test]
    fn longest_balanced_span_beats_an_earlier_example_object() {
        let reply = "For example {\"a\":1}. The actual handoff is \
                     {\"status\":\"ok\",\"summary\":\"secret vaulted\"}";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(source, ReplyJsonSource::ProseSpan);
    }

    #[test]
    fn array_payload_is_recovered_when_there_is_no_object() {
        let reply = "The findings are:\n[{\"id\":1},{\"id\":2}]";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(source, ReplyJsonSource::ProseSpan);
    }

    #[test]
    fn pure_prose_yields_nothing() {
        assert!(extract("The artifact contains only moltbook_agent.py — no test files.").is_none());
    }

    #[test]
    fn prose_with_unbalanced_brace_yields_nothing() {
        assert!(extract("Use the {placeholder syntax for now.").is_none());
    }

    #[test]
    fn empty_reply_yields_nothing() {
        assert!(extract("").is_none());
        assert!(extract("   \n  ").is_none());
    }

    #[test]
    fn think_block_is_removed_before_parsing() {
        let reply = "<think>maybe {\"status\":\"guess\"}</think>{\"status\":\"ok\"}";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(source, ReplyJsonSource::Whole);
    }

    #[test]
    fn think_block_plus_prose_wrapped_json() {
        let reply = "<think>reasoning</think>All set. {\"status\":\"ok\"}";
        let (v, source) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(source, ReplyJsonSource::ProseSpan);
    }

    #[test]
    fn multibyte_prose_around_the_payload_is_safe() {
        let reply = "Cérémonie terminée ✅ — voici le handoff :\n{\"status\":\"ok\"}\nÀ bientôt 🎉";
        let (v, _) = extract(reply).unwrap();
        assert_eq!(v["status"], "ok");
    }

    #[test]
    fn span_candidate_cap_is_bounded() {
        // Unbalanced openers past the cap: a violation rather than an
        // unbounded scan.
        let reply = format!("{} tail", "{".repeat(20));
        assert!(extract(&reply).is_none());
    }

    #[test]
    fn strip_think_blocks_removes_closed_block() {
        let input = "<think>reasoning here</think>{\"status\":\"ok\"}";
        assert_eq!(strip_think_blocks(input).as_ref(), "{\"status\":\"ok\"}");
    }

    #[test]
    fn strip_think_blocks_removes_unclosed_block() {
        let input = "hello<think>never closed";
        assert_eq!(strip_think_blocks(input).as_ref(), "hello");
    }

    #[test]
    fn strip_think_blocks_preserves_text_without_think_tags() {
        let input = "plain reply";
        assert_eq!(strip_think_blocks(input).as_ref(), input);
    }

    /// The trim is part of the contract in both directions: a reply padded with
    /// whitespace must come back the same shape whether or not the model
    /// happened to wrap a think block around it.
    #[test]
    fn strip_think_blocks_trims_with_and_without_tags() {
        assert_eq!(strip_think_blocks("\n  plain reply \n").as_ref(), "plain reply");
        assert_eq!(
            strip_think_blocks("\n <think>a</think> plain reply \n").as_ref(),
            "plain reply"
        );
        // The no-tag path stays allocation-free.
        assert!(matches!(
            strip_think_blocks("  padded  "),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn strip_think_blocks_handles_multiple_blocks() {
        let input = "hello<think>a</think> <think>b</think>world";
        assert_eq!(strip_think_blocks(input).as_ref(), "hello world");
    }
}
