//! Canary test for the egress chokepoint (RFC data-envelopes §5.2).
//!
//! The proof for phase 1b slice 1: a `local_only` tool result's canary marker
//! **never appears in any captured remote-provider request body**. This turns
//! the §5.2.3 outbound assertion from a tripwire into a proof over the full
//! filtering + serialization path.
//!
//! Drives [`EgressChokepointDriver`] directly around a mock inner driver that
//! captures every `CompletionRequest` it sees. No full session spun up — the
//! wrapper is the unit under test, and the wire-body capture is the proof
//! surface. A session-level canary lands once the full chokepoint (slice 2:
//! msg_<ulid> binding, compression eligibility, filtered wire view) is in.
//!
//! Covers RFC §5.6 scenario step 4 (asking a remote LLM about a local-only
//! summary must not leak it) and the local→remote failover leak closure.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use autonoetic_gateway::llm::egress_chokepoint::{compute_filter_report, EgressChokepointDriver};
use autonoetic_gateway::llm::{
    CompletionRequest, CompletionResponse, LlmDriver, Message, Role, StopReason, TokenUsage,
};
use autonoetic_types::egress::{EgressLabel, Sink};

/// A mock inner driver that captures every request body it receives, and can
/// be configured to fail (to exercise failover).
struct CapturingDriver {
    captures: Mutex<Vec<CompletionRequest>>,
    /// When true, `complete` returns an error (simulating a transient primary
    /// failure so a failover preset gets tried).
    fail: bool,
}

impl CapturingDriver {
    fn new() -> Self {
        Self {
            captures: Mutex::new(Vec::new()),
            fail: false,
        }
    }
    fn failing() -> Self {
        Self {
            captures: Mutex::new(Vec::new()),
            fail: true,
        }
    }
    fn captures(&self) -> Vec<CompletionRequest> {
        self.captures.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmDriver for CapturingDriver {
    async fn complete(&self, request: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        self.captures.lock().unwrap().push(request.clone());
        if self.fail {
            anyhow::bail!("simulated transient failure (failover trigger)");
        }
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

const CANARY: &str = "CANARY-SECRET-MARKER-9b3f7c";

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

fn assistant_msg(content: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
        reasoning_content: None,
        reasoning_details: None,
    }
}

/// Serialize a request the way the provider wire would — every message's
/// content concatenated. The canary assertion is that the marker does not
/// appear in this serialization.
fn wire_body(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn req_with_labels(messages: Vec<Message>, labels: HashMap<String, EgressLabel>) -> CompletionRequest {
    let mut meta = HashMap::new();
    meta.insert(
        autonoetic_gateway::llm::egress_chokepoint::EGRESS_LABELS_KEY.to_string(),
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

// ── The core proof ─────────────────────────────────────────────────────

/// RFC §5.2 / acceptance criterion #1: a `local_only` tool result's content
/// never reaches a remote provider's request body. The canary marker must be
/// absent; the indication must be present.
#[tokio::test]
async fn local_only_canary_absent_from_remote_body() {
    let inner = Arc::new(CapturingDriver::new());
    let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
    let mut labels = HashMap::new();
    labels.insert("tc_email".to_string(), EgressLabel::local_only());
    // A realistic conversation: user asks about emails, tool reads them
    // (local_only), assistant now composing the next turn on a REMOTE preset.
    let messages = vec![
        user_msg("summarize my recent emails"),
        // The tool result carries the canary — this is what must NOT leak.
        tool_msg(
            "tc_email",
            &format!("{{\"subject\":\"RESET YOUR PASSWORD\", \"body\":\"code {CANARY}\"}}"),
        ),
        assistant_msg("I've read your emails."),
        user_msg("what did they say?"),
    ];
    let req = req_with_labels(messages, labels);

    wrapper.complete(&req).await.unwrap();

    let caps = inner.captures();
    assert_eq!(caps.len(), 1, "exactly one capture");
    let body = wire_body(&caps[0]);
    assert!(
        !body.contains(CANARY),
        "CANARY LEAKED to remote provider body:\n{body}"
    );
    // The indication must appear so the remote model keeps coherent context.
    assert!(
        body.contains("[withheld:"),
        "indication missing from remote body:\n{body}"
    );
    assert!(body.contains("local_only"));
}

/// The local model legitimately sees `local_only` content — no over-withholding.
#[tokio::test]
async fn local_only_canary_present_for_local_sink() {
    let inner = Arc::new(CapturingDriver::new());
    let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::LocalModel);
    let mut labels = HashMap::new();
    labels.insert("tc_email".to_string(), EgressLabel::local_only());
    let req = req_with_labels(
        vec![
            user_msg("summarize my emails"),
            tool_msg("tc_email", &format!("email body {CANARY}")),
        ],
        labels,
    );

    wrapper.complete(&req).await.unwrap();
    let body = wire_body(&inner.captures()[0]);
    assert!(
        body.contains(CANARY),
        "local sink should see local_only content, got:\n{body}"
    );
}

/// `unrestricted` content passes to any sink.
#[tokio::test]
async fn unrestricted_canary_passes_to_remote() {
    let inner = Arc::new(CapturingDriver::new());
    let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
    let mut labels = HashMap::new();
    labels.insert("tc_public".to_string(), EgressLabel::unrestricted());
    let req = req_with_labels(
        vec![tool_msg("tc_public", &format!("public data {CANARY}"))],
        labels,
    );

    wrapper.complete(&req).await.unwrap();
    let body = wire_body(&inner.captures()[0]);
    assert!(body.contains(CANARY), "unrestricted content should pass");
}

// ── Failover leak closure ──────────────────────────────────────────────

/// RFC §5.2: the cross-provider failover loop re-ships the same request to a
/// *different* provider — including from local to remote. The wrapper is
/// installed per-driver via `build_driver`, so each fallback re-filters against
/// its own sink. This test simulates: primary LOCAL fails, fallback REMOTE
/// receives the request — the canary must STILL be absent.
#[tokio::test]
async fn failover_local_to_remote_still_withholds() {
    // Primary: local, fails (transient).
    let local_inner = Arc::new(CapturingDriver::failing());
    let local_wrapper =
        EgressChokepointDriver::new(local_inner.clone(), Sink::LocalModel);
    // Fallback: remote, succeeds.
    let remote_inner = Arc::new(CapturingDriver::new());
    let remote_wrapper =
        EgressChokepointDriver::new(remote_inner.clone(), Sink::RemoteModel);

    let mut labels = HashMap::new();
    labels.insert("tc_email".to_string(), EgressLabel::local_only());
    // The SAME request is re-shipped to the fallback (RFC §1 problem bullet 3
    // — confirmed by exploration: lifecycle.rs reuses `&req` across fallbacks).
    let req = req_with_labels(
        vec![tool_msg("tc_email", &format!("secret {CANARY} data"))],
        labels.clone(),
    );

    // Primary (local) — would see the content, but fails.
    let primary = local_wrapper.complete(&req).await;
    assert!(primary.is_err(), "primary should fail (failover trigger)");
    // The local sink DID see the canary (legitimately) before failing.
    assert!(
        wire_body(&local_inner.captures()[0]).contains(CANARY),
        "local primary legitimately sees local_only content"
    );

    // Fallback (remote) — must NOT see the canary.
    let _ = remote_wrapper.complete(&req).await.unwrap();
    let remote_body = wire_body(&remote_inner.captures()[0]);
    assert!(
        !remote_body.contains(CANARY),
        "CANARY LEAKED via local→remote failover:\n{remote_body}"
    );
    assert!(remote_body.contains("[withheld:"), "indication missing in fallback");
}

// ── Filter report + assertion ──────────────────────────────────────────

/// The filter report correctly accounts withheld vs included envelopes.
#[tokio::test]
async fn filter_report_counts_withheld_and_included() {
    let mut labels = HashMap::new();
    labels.insert("tc_secret".to_string(), EgressLabel::local_only());
    labels.insert("tc_public".to_string(), EgressLabel::unrestricted());
    let req = req_with_labels(
        vec![
            tool_msg("tc_secret", &format!("secret {CANARY}")),
            tool_msg("tc_public", "public"),
        ],
        labels,
    );
    let report = compute_filter_report(&req, Sink::RemoteModel);
    assert_eq!(report.withheld.len(), 1);
    assert_eq!(report.withheld[0].tool_call_id, "tc_secret");
    assert_eq!(report.included, 1); // tc_public passed
    assert!(!report.has_violations());
}

/// RFC §5.2.3 tripwire: a non-withheld message echoing withheld content
/// verbatim is flagged as an assertion violation. (Tripwire, not proof — the
/// canary test above carries the proof burden per RFC §11.)
#[tokio::test]
async fn assertion_violation_flagged_on_verbatim_echo() {
    let mut labels = HashMap::new();
    labels.insert("tc_secret".to_string(), EgressLabel::local_only());
    // The user message echoes the canary verbatim — the kind of leak the
    // assertion is meant to catch (echo exfil).
    let req = req_with_labels(
        vec![
            tool_msg("tc_secret", CANARY),
            user_msg(&format!("the data was: {CANARY}")),
        ],
        labels,
    );
    let report = compute_filter_report(&req, Sink::RemoteModel);
    assert!(report.has_violations(), "verbatim echo must trigger a violation");
    assert_eq!(report.violations[0].tool_call_id, "tc_secret");
}

/// The indication itself contains no bytes of the withheld content.
#[tokio::test]
async fn indication_carries_no_content_bytes() {
    let inner = Arc::new(CapturingDriver::new());
    let wrapper = EgressChokepointDriver::new(inner.clone(), Sink::RemoteModel);
    let mut labels = HashMap::new();
    labels.insert("tc_secret".to_string(), EgressLabel::local_only());
    // Distinctive substrings that must not survive into the indication.
    let secret = "SUPER-UNIQUE-PASSWORD-zzz999";
    let req = req_with_labels(vec![tool_msg("tc_secret", secret)], labels);

    wrapper.complete(&req).await.unwrap();
    let body = wire_body(&inner.captures()[0]);
    assert!(!body.contains(secret));
    assert!(!body.contains("zzz999"));
    assert!(!body.contains("PASSWORD"));
}
