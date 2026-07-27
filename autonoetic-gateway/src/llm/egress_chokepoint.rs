//! Egress chokepoint driver — RFC data-envelopes §5.2.
//!
//! A policy-wrapping [`LlmDriver`] that makes withholding real at the LLM
//! boundary. Every provider completion (primary + every failover preset) goes
//! through this wrapper, installed by [`crate::llm::build_driver`].
//!
//! ## What it does
//!
//! For each completion request, the wrapper:
//! 1. Reads the `tool_call_id → EgressLabel` map from
//!    [`CompletionRequest::metadata`] under [`EGRESS_LABELS_KEY`]. Absent/empty
//!    → unconfigured deployment, pass through unchanged (zero-cost fast path).
//! 2. For each `Role::Tool` message whose label excludes the target [`Sink`]
//!    (the provider's class), replaces `content` with a non-divulging
//!    [`Indication`] built from metadata only (RFC §3.3). Records the
//!    withholding in a [`FilterReport`].
//! 3. **Outbound content assertion (RFC §5.2.3):** for each withheld payload,
//!    asserts it does not appear verbatim in any *non-withheld* message of the
//!    filtered request. A hit is a bug or an echo-exfil attempt — recorded as
//!    an assertion violation. This is a tripwire, not a proof (RFC §11); the
//!    canary test carries the proof burden.
//! 4. Stashes the [`FilterReport`] into the filtered request's metadata under
//!    [`EGRESS_FILTER_REPORT_KEY`], then forwards to the inner driver.
//!
//! ## Why a driver wrapper (not lifecycle.rs)
//!
//! `complete(&CompletionRequest)` is by-reference → the wrapper clones, filters,
//! forwards with no trait change. `build_driver` constructs the driver for the
//! primary completion AND every fallback preset in the failover loop, so
//! wrapping there covers all paths uniformly — closing the local→remote
//! failover leak for free. `stream()`'s default impl wraps `complete()`, so
//! overriding `complete` alone covers both.
//!
//! ## Why the label map rides in metadata (not a store handle)
//!
//! `build_driver` takes only `(LlmConfig, client)` — no `GatewayStore`, and 11
//! call sites (most auxiliary: capsule, digest, outcome-writer). Forcing a
//! store through all of them is wrong: the chokepoint only matters for the main
//! reasoning loop. Lifecycle.rs attaches the label map (already maintained by
//! the 1c labeler) into `req.metadata` at assembly time; the wrapper is
//! stateless except for the target sink. Causal events are emitted by
//! lifecycle.rs after the call (it has the store + session context), keeping
//! the driver pure and unit-testable.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use autonoetic_types::egress::{EgressLabel, Indication, IndicationVerbosity, Sink};

use crate::llm::{CompletionRequest, CompletionResponse, LlmDriver};

/// Metadata key: a JSON-serialized `HashMap<String, EgressLabel>` keyed by
/// `tool_call_id`. Attached by lifecycle.rs at request-assembly time when the
/// session's labeler has labeled any tool results in the assembled messages.
/// Absent or empty → unconfigured deployment, wrapper is a no-op.
pub const EGRESS_LABELS_KEY: &str = "__egress_labels";

/// Metadata key: a JSON-serialized [`FilterReport`] the wrapper stashes onto
/// the cloned, filtered request it forwards to the inner driver. This lets
/// tests and the [`FilterReport::extract`] helper observe what the wrapper did
/// to a request. **Note:** lifecycle.rs does NOT read the report from the
/// forwarded request — it cannot, since `complete(&CompletionRequest)` is
/// by-reference and the wrapper's filtered clone is internal. Instead,
/// lifecycle.rs re-derives the report from the original request via
/// [`compute_filter_report`] after the call returns, and emits the
/// `egress.envelope_withheld` / `egress.request_filtered` /
/// `egress.assertion_violation` causal events from that. The two reports agree
/// because filtering is a pure function of (request × labels × sink).
pub const EGRESS_FILTER_REPORT_KEY: &str = "__egress_filter_report";

/// The number of recent messages to bound the verbatim-echo assertion to
/// (RFC §11 — O(n·m) naively; bound to recent turns). Generous default that
/// covers a typical tool-result + assistant-reasoning window.
const ASSERTION_WINDOW_MESSAGES: usize = 40;

/// One withheld envelope, for the filter report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WithheldEntry {
    /// The `tool_call_id` of the withheld tool-result message.
    pub tool_call_id: String,
    /// The label that excluded the target sink.
    pub label: EgressLabel,
    /// The indication text that replaced the content.
    pub indication: String,
}

/// An assertion violation: a withheld payload appeared verbatim in a
/// non-withheld message of the filtered request (RFC §5.2.3 tripwire).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AssertionViolation {
    /// The `tool_call_id` of the envelope whose content leaked.
    pub tool_call_id: String,
    /// SHA-256 → 12 hex chars of the leaked payload (for audit correlation).
    pub payload_digest: String,
    /// Index of the message in which the leak was detected.
    pub found_in_message_index: usize,
}

/// Report of what the wrapper did to a request, stashed into metadata for the
/// caller (lifecycle.rs) to turn into causal events.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FilterReport {
    /// The sink this request was filtered against.
    pub sink: String,
    /// Tool-result messages whose content was replaced with an indication.
    pub withheld: Vec<WithheldEntry>,
    /// Tool-result messages whose label permitted the sink (passed through).
    pub included: usize,
    /// Assertion violations detected (verbatim echo of withheld content).
    pub violations: Vec<AssertionViolation>,
}

impl FilterReport {
    /// Whether any content was withheld.
    pub fn withheld_any(&self) -> bool {
        !self.withheld.is_empty()
    }

    /// Whether any assertion violation fired.
    pub fn has_violations(&self) -> bool {
        !self.violations.is_empty()
    }

    /// Deserialize a `FilterReport` from a request's metadata, if present.
    /// Returns `None` when the wrapper didn't run or produced no report.
    pub fn extract(req: &CompletionRequest) -> Option<Self> {
        let meta = req.metadata.as_ref()?;
        let val = meta.get(EGRESS_FILTER_REPORT_KEY)?;
        serde_json::from_value(val.clone()).ok()
    }
}

/// The egress chokepoint driver. Wraps an inner [`LlmDriver`] and filters
/// outbound content per the label plane.
pub struct EgressChokepointDriver {
    inner: Arc<dyn LlmDriver>,
    sink: Sink,
    verbosity: IndicationVerbosity,
}

impl std::fmt::Debug for EgressChokepointDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressChokepointDriver")
            .field("sink", &self.sink)
            .field("verbosity", &self.verbosity)
            .finish_non_exhaustive()
    }
}

impl EgressChokepointDriver {
    /// Construct a wrapper around `inner` that filters against `sink`.
    pub fn new(inner: Arc<dyn LlmDriver>, sink: Sink) -> Self {
        Self {
            inner,
            sink,
            verbosity: IndicationVerbosity::Descriptive,
        }
    }

    /// Construct with an explicit verbosity (session-policy knob, RFC §3.3).
    pub fn with_verbosity(
        inner: Arc<dyn LlmDriver>,
        sink: Sink,
        verbosity: IndicationVerbosity,
    ) -> Self {
        Self {
            inner,
            sink,
            verbosity,
        }
    }

    /// Read the label map from a request's metadata.
    ///
    /// - `Ok(map)` when the key is present and parses (the map may be empty).
    /// - `Ok(None)` when the key is **absent** — an unconfigured deployment or
    ///   an auxiliary LLM call (capsule/digest) that carries no labels. The
    ///   wrapper's fast path fires; no filtering, no fail.
    /// - `Err` when the key is **present but malformed**. This is fail-closed:
    ///   a corrupt label map on a security boundary is treated as a bug or an
    ///   attack, not silently bypassed (RFC §2.2). The caller aborts the call.
    fn read_label_map(
        req: &CompletionRequest,
    ) -> anyhow::Result<Option<HashMap<String, EgressLabel>>> {
        let Some(meta) = req.metadata.as_ref() else {
            return Ok(None);
        };
        let Some(val) = meta.get(EGRESS_LABELS_KEY) else {
            return Ok(None);
        };
        // The map serializes as a JSON object { tool_call_id: [sinks...] }.
        let map = serde_json::from_value::<HashMap<String, EgressLabel>>(val.clone())
            .map_err(|e| {
                anyhow::anyhow!(
                    "egress label map (__egress_labels) is present but malformed: {e} — \
                     aborting completion (fail-closed)"
                )
            })?;
        Ok(Some(map))
    }

    /// Core filter: given a request + label map + target sink, produce a
    /// filtered clone of the request plus a [`FilterReport`]. Pure function of
    /// (request × labels × sink) — RFC §5.2. Exposed for unit testing.
    fn filter_request(
        &self,
        req: &CompletionRequest,
        labels: &HashMap<String, EgressLabel>,
    ) -> (CompletionRequest, FilterReport) {
        let mut filtered = req.clone();
        let mut report = FilterReport {
            sink: sink_name(self.sink),
            withheld: Vec::new(),
            included: 0,
            violations: Vec::new(),
        };

        if labels.is_empty() {
            // Unconfigured: nothing to filter. Stash an (empty) report so the
            // caller can still observe the wrapper ran.
            stash_report(&mut filtered, &report);
            return (filtered, report);
        }

        // Pass 1: substitute indications for withheld tool-result messages.
        // Collect (index, original_content) for the assertion pass.
        let mut withheld_payloads: Vec<(usize, String, String)> = Vec::new();
        // tool_call_id → count, for grouping indications by tool (RFC §3.3
        // supports "2× email.read results"). Phase-1b slice keeps one
        // indication per envelope (per-issue #905 open question 5).
        //
        // Build a tool_call_id → tool_name index from assistant messages'
        // tool_calls, so the indication can name the tool (RFC §3.3) without
        // reading any payload content — the (id, name) pair is metadata.
        // Collected into owned strings so the immutable borrow of
        // `filtered.messages` ends before the mutable filter pass below.
        let tool_names: std::collections::HashMap<String, String> = filtered
            .messages
            .iter()
            .flat_map(|m| {
                m.tool_calls
                    .iter()
                    .map(|tc| (tc.id.clone(), tc.name.clone()))
            })
            .collect();
        for (idx, msg) in filtered.messages.iter_mut().enumerate() {
            if msg.role != crate::llm::Role::Tool {
                continue;
            }
            let Some(tc_id) = msg.tool_call_id.as_ref() else {
                continue;
            };
            let Some(label) = labels.get(tc_id) else {
                // Tool result with no label → unrestricted default → passes.
                report.included += 1;
                continue;
            };
            if label.allows(self.sink) {
                report.included += 1;
                continue;
            }
            // Withhold: replace content with an indication built from metadata.
            // The tool name is derived from the matching assistant tool_call —
            // metadata only, never the payload.
            let tool_name = tool_names.get(tc_id).map(|s| s.as_str());
            let original = std::mem::take(&mut msg.content);
            let indication = Indication::generate(tool_name, 1, label, self.verbosity);
            msg.content = indication.text.clone();
            report.withheld.push(WithheldEntry {
                tool_call_id: tc_id.clone(),
                label: label.clone(),
                indication: indication.text,
            });
            withheld_payloads.push((idx, original, tc_id.clone()));
        }

        // Pass 2: outbound content assertion (RFC §5.2.3). For each withheld
        // payload, check it doesn't appear verbatim in any non-withheld message
        // of the filtered request. Bounded to recent N messages. Tripwire, not
        // proof — the canary test carries the proof burden.
        if !withheld_payloads.is_empty() {
            let window_start = filtered.messages.len().saturating_sub(ASSERTION_WINDOW_MESSAGES);
            for (_, payload, tc_id) in &withheld_payloads {
                if payload.is_empty() {
                    continue;
                }
                for (idx, msg) in filtered.messages.iter().enumerate() {
                    if idx < window_start {
                        continue;
                    }
                    // Skip the message the payload came from (already replaced).
                    if msg.role == crate::llm::Role::Tool
                        && msg
                            .tool_call_id
                            .as_ref()
                            .map(|id| id == tc_id)
                            .unwrap_or(false)
                    {
                        continue;
                    }
                    if msg.content.contains(payload.as_str()) {
                        report.violations.push(AssertionViolation {
                            tool_call_id: tc_id.clone(),
                            payload_digest: autonoetic_types::id_format::hash_and_truncate(
                                payload, 12,
                            ),
                            found_in_message_index: idx,
                        });
                        break; // one hit per payload is enough
                    }
                }
            }
        }

        stash_report(&mut filtered, &report);
        (filtered, report)
    }
}

#[async_trait]
impl LlmDriver for EgressChokepointDriver {
    async fn complete(&self, request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        // Fail closed on a malformed label map (RFC §2.2): a corrupt map on a
        // security boundary is a bug or an attack, not silently bypassed.
        let labels = match Self::read_label_map(request)? {
            None => {
                // No label map attached → unconfigured deployment or auxiliary
                // LLM call. Zero-cost pass-through (no clone, no filter).
                return self.inner.complete(request).await;
            }
            Some(map) if map.is_empty() => {
                // Empty map → nothing to filter. Pass through.
                return self.inner.complete(request).await;
            }
            Some(map) => map,
        };
        let (filtered, report) = self.filter_request(request, &labels);
        // Fail closed on an outbound-assertion violation (RFC §5.2.3): a
        // withheld payload appearing verbatim in a non-withheld message is a
        // bug or an echo-exfil attempt — never send the request to the
        // provider. The violation is already recorded in the report; lifecycle
        // re-derives the report from the original request (via
        // `compute_filter_report`) to emit the causal event even on this abort.
        if report.has_violations() {
            tracing::error!(
                target: "egress_chokepoint",
                sink = %report.sink,
                violation_count = report.violations.len(),
                "egress assertion violation — aborting completion (fail-closed)"
            );
            return Err(anyhow::anyhow!(
                "egress assertion violation: withheld payload appeared verbatim in a \
                 non-withheld message — completion aborted (fail-closed, RFC §5.2.3)"
            ));
        }
        self.inner.complete(&filtered).await
    }
}

/// Compute a [`FilterReport`] for a request without forwarding it — used by
/// lifecycle.rs after a completion returns to emit the chokepoint causal events
/// (RFC §9.1). Re-derivation: the driver already ran the filter internally and
/// forwarded a filtered clone; this re-runs the *report-only* pass so the
/// caller can audit what was withheld. Cheap (single pass over messages, and
/// the common case — no labels attached — returns an empty report immediately).
///
/// `sink` is the target sink of the provider that handled the completion
/// (known from the routed preset's `egress_class`).
pub fn compute_filter_report(
    req: &CompletionRequest,
    sink: Sink,
) -> FilterReport {
    // Best-effort audit: a malformed map here is logged but not fatal (the
    // completion already aborted in the driver if it was malformed; this is the
    // post-hoc report path). Absent or empty → empty report.
    let labels = match EgressChokepointDriver::read_label_map(req) {
        Ok(Some(m)) if !m.is_empty() => m,
        Ok(_) => {
            return FilterReport {
                sink: sink_name(sink),
                ..Default::default()
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "egress_chokepoint",
                error = %e,
                "malformed label map in compute_filter_report — emitting empty report"
            );
            return FilterReport {
                sink: sink_name(sink),
                ..Default::default()
            };
        }
    };
    // Reuse the driver's pure filter logic with descriptive verbosity (the
    // report's indication strings are informational; verbosity is a
    // session-policy knob that lives on the session, not the report helper).
    let dummy = EgressChokepointDriver {
        inner: Arc::new(NoopDriver),
        sink,
        verbosity: IndicationVerbosity::Descriptive,
    };
    dummy.filter_request(req, &labels).1
}

// A zero-instance inner driver used only by `compute_filter_report`'s temporary
// wrapper. Never actually called (the report helper discards the filtered
// request). Kept minimal to satisfy the `Arc<dyn LlmDriver>` field.
struct NoopDriver;

#[async_trait]
impl LlmDriver for NoopDriver {
    async fn complete(&self, _request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        unreachable!("NoopDriver is never called; compute_filter_report discards the filtered request")
    }
}

/// Stash a `FilterReport` into a request's metadata (creating the metadata map
/// if absent). Mutates the request in place.
fn stash_report(req: &mut CompletionRequest, report: &FilterReport) {
    let meta = req
        .metadata
        .get_or_insert_with(|| HashMap::new());
    meta.insert(
        EGRESS_FILTER_REPORT_KEY.to_string(),
        serde_json::to_value(report).unwrap_or(serde_json::Value::Null),
    );
}

fn sink_name(s: Sink) -> String {
    match s {
        Sink::LocalModel => "local_model".to_string(),
        Sink::RemoteModel => "remote_model".to_string(),
        Sink::LocalAgent => "local_agent".to_string(),
        Sink::FederatedAgent => "federated_agent".to_string(),
        Sink::Network => "network".to_string(),
        Sink::MemoryPersist => "memory_persist".to_string(),
        Sink::UserReply => "user_reply".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CompletionRequest, CompletionResponse, Message, Role, StopReason, TokenUsage};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A mock inner driver that captures every request it sees.
    struct CapturingDriver {
        captured: Mutex<Vec<CompletionRequest>>,
    }

    impl CapturingDriver {
        fn new() -> Self {
            Self {
                captured: Mutex::new(Vec::new()),
            }
        }
        fn captures(&self) -> Vec<CompletionRequest> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmDriver for CapturingDriver {
        async fn complete(&self, request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
            self.captured.lock().unwrap().push(request.clone());
            Ok(CompletionResponse {
                text: "ok".to_string(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                reasoning_content: None,
                reasoning_details: None,
            })
        }
    }

    fn tool_msg(id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: Some(id.to_string()),
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    fn user_msg(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    fn req_with_labels(messages: Vec<Message>, labels: HashMap<String, EgressLabel>) -> CompletionRequest {
        let mut meta = HashMap::new();
        meta.insert(
            EGRESS_LABELS_KEY.to_string(),
            serde_json::to_value(&labels).unwrap(),
        );
        CompletionRequest {
            model: "test".to_string(),
            messages,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: Some(meta),
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        }
    }

    const CANARY: &str = "CANARY-SECRET-MARKER";

    // ── Inert / pass-through ──────────────────────────────────────────────

    #[tokio::test]
    async fn no_labels_is_zero_cost_passthrough() {
        // No metadata at all → wrapper must not clone, just forward.
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let req = CompletionRequest {
            model: "test".into(),
            messages: vec![tool_msg("tc_1", CANARY)],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: None,
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        };
        wrapper.complete(&req).await.unwrap();
        let caps = inner.captures();
        assert_eq!(caps.len(), 1);
        // Canary present — nothing filtered.
        assert!(caps[0].messages[0].content.contains(CANARY));
    }

    #[tokio::test]
    async fn empty_label_map_is_passthrough() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let req = req_with_labels(vec![tool_msg("tc_1", CANARY)], HashMap::new());
        wrapper.complete(&req).await.unwrap();
        let caps = inner.captures();
        assert_eq!(caps.len(), 1);
        assert!(caps[0].messages[0].content.contains(CANARY));
    }

    // ── Withholding ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn local_only_withheld_from_remote_sink() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let req = req_with_labels(vec![tool_msg("tc_secret", CANARY)], labels);

        wrapper.complete(&req).await.unwrap();
        let caps = inner.captures();
        assert_eq!(caps.len(), 1);
        // The canary must NOT appear in the captured (filtered) request.
        let body: String = caps[0]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            !body.contains(CANARY),
            "canary leaked to remote sink: {body}"
        );
        // The indication must appear instead.
        assert!(body.contains("[withheld:"), "indication missing: {body}");
        assert!(body.contains("local_only"));
    }

    #[tokio::test]
    async fn local_only_passes_to_local_sink() {
        // The local model legitimately sees local_only content.
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::LocalModel);
        let mut labels = HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let req = req_with_labels(vec![tool_msg("tc_secret", CANARY)], labels);

        wrapper.complete(&req).await.unwrap();
        let caps = inner.captures();
        assert_eq!(caps.len(), 1);
        assert!(
            caps[0].messages[0].content.contains(CANARY),
            "local sink should see local_only content"
        );
    }

    #[tokio::test]
    async fn unrestricted_passes_to_any_sink() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_clean".to_string(), EgressLabel::unrestricted());
        let req = req_with_labels(vec![tool_msg("tc_clean", "public data")], labels);

        wrapper.complete(&req).await.unwrap();
        let caps = inner.captures();
        assert_eq!(caps.len(), 1);
        assert!(caps[0].messages[0].content.contains("public data"));
    }

    #[tokio::test]
    async fn unlabeled_tool_result_passes_through() {
        // A tool result with no entry in the label map → unrestricted default.
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        // Label exists for a different tool_call_id; tc_other is unlabeled.
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let req = req_with_labels(vec![tool_msg("tc_other", "unlabeled data")], labels);

        wrapper.complete(&req).await.unwrap();
        let caps = inner.captures();
        assert_eq!(caps.len(), 1);
        assert!(caps[0].messages[0].content.contains("unlabeled data"));
    }

    // ── Indication contains no content bytes ──────────────────────────────

    #[tokio::test]
    async fn indication_contains_no_content_bytes() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        // Content with distinctive substrings that must not survive.
        let secret = "MY-SECRET-VALUE-12345";
        let req = req_with_labels(vec![tool_msg("tc_secret", secret)], labels);

        wrapper.complete(&req).await.unwrap();
        let body: String = inner.captures()[0]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(!body.contains(secret));
        assert!(!body.contains("12345"));
        assert!(!body.contains("MY-SECRET"));
    }

    // ── Outbound content assertion (RFC §5.2.3 tripwire) ──────────────────

    #[tokio::test]
    async fn assertion_catches_verbatim_echo_in_user_message() {
        // A non-withheld user message echoes the withheld payload verbatim.
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let messages = vec![
            tool_msg("tc_secret", CANARY),
            user_msg(&format!("here is the data: {CANARY}")),
        ];
        let req = req_with_labels(messages, labels);

        // Run the filter directly to inspect the report.
        let labels_read = EgressChokepointDriver::read_label_map(&req)
            .expect("labels parse")
            .unwrap_or_default();
        let (_filtered, report) = wrapper.filter_request(&req, &labels_read);
        assert!(report.has_violations(), "echo should trigger a violation");
        assert_eq!(report.violations[0].tool_call_id, "tc_secret");
        // The leak is still filtered (the tool message is withheld), but the
        // violation is flagged so the caller can refuse/audit.
    }

    #[tokio::test]
    async fn assertion_does_not_fire_for_unrelated_content() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let messages = vec![
            tool_msg("tc_secret", CANARY),
            user_msg("an unrelated user message"),
        ];
        let req = req_with_labels(messages, labels);
        let labels_read = EgressChokepointDriver::read_label_map(&req)
            .expect("labels parse")
            .unwrap_or_default();
        let (_filtered, report) = wrapper.filter_request(&req, &labels_read);
        assert!(!report.has_violations());
        assert!(report.withheld_any());
    }

    // ── Filter report ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn filter_report_stashed_into_metadata() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        let req = req_with_labels(vec![tool_msg("tc_secret", CANARY)], labels);

        wrapper.complete(&req).await.unwrap();
        let caps = inner.captures();
        let report = FilterReport::extract(&caps[0]).expect("report stashed");
        assert_eq!(report.sink, "remote_model");
        assert_eq!(report.withheld.len(), 1);
        assert_eq!(report.withheld[0].tool_call_id, "tc_secret");
        assert_eq!(report.withheld[0].label, EgressLabel::local_only());
        assert!(!report.withheld[0].indication.contains(CANARY));
    }

    // ── Fail-closed (RFC §2.2 / §5.2.3) ───────────────────────────────────

    /// A malformed label map (present but unparseable) must fail closed — the
    /// completion aborts rather than silently bypassing the chokepoint.
    #[tokio::test]
    async fn malformed_label_map_fails_closed() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut meta = HashMap::new();
        // Malformed: not a valid label map (a string, not an object).
        meta.insert(
            EGRESS_LABELS_KEY.to_string(),
            serde_json::Value::String("not-a-map".to_string()),
        );
        let req = CompletionRequest {
            model: "test".into(),
            messages: vec![tool_msg("tc_1", CANARY)],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: Some(meta),
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        };

        let result = wrapper.complete(&req).await;
        assert!(
            result.is_err(),
            "malformed label map must fail closed, not bypass"
        );
        // And the inner driver was never called — no request forwarded.
        assert!(
            inner.captures().is_empty(),
            "no request should be forwarded on malformed-map abort"
        );
    }

    /// An outbound-assertion violation (verbatim echo of a withheld payload in
    /// a non-withheld message) must abort the completion — the request is
    /// never sent to the provider. (RFC §5.2.3 fail-closed.)
    #[tokio::test]
    async fn assertion_violation_aborts_completion() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_secret".to_string(), EgressLabel::local_only());
        // The user message echoes the canary verbatim — the violation.
        let messages = vec![
            tool_msg("tc_secret", CANARY),
            user_msg(&format!("the data was: {CANARY}")),
        ];
        let req = req_with_labels(messages, labels);

        let result = wrapper.complete(&req).await;
        assert!(
            result.is_err(),
            "assertion violation must abort the completion (fail-closed)"
        );
        assert!(
            inner.captures().is_empty(),
            "no request should be forwarded on assertion violation"
        );
    }

    // ── Tool-name derivation (RFC §3.3, no content read) ─────────────────

    /// The indication names the tool, derived from the matching assistant
    /// tool_call's `name` by `tool_call_id` — metadata only, never the payload.
    #[tokio::test]
    async fn indication_names_tool_from_assistant_tool_call() {
        let inner = Arc::new(CapturingDriver::new());
        let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
        let mut labels = HashMap::new();
        labels.insert("tc_email".to_string(), EgressLabel::local_only());
        // An assistant message carrying the tool_call (id, name) pair, followed
        // by the tool result. The wrapper joins on tool_call_id to find "email.read".
        let messages = vec![
            user_msg("read my emails"),
            Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![crate::llm::ToolCall {
                    id: "tc_email".to_string(),
                    name: "email.read".to_string(),
                    arguments: "{}".to_string(),
                }],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
            tool_msg("tc_email", CANARY),
        ];
        let req = req_with_labels(messages, labels);

        wrapper.complete(&req).await.unwrap();
        let body: String = inner.captures()[0]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            !body.contains(CANARY),
            "canary must not leak"
        );
        // The indication names the tool — better model coherence (RFC §3.3).
        assert!(
            body.contains("email.read"),
            "indication should name the tool 'email.read': {body}"
        );
        assert!(body.contains("local_only"));
    }
}
