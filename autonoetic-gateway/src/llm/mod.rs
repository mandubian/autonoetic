//! LLM Driver Abstraction and Types.
//!
//! Provides a thin, unified interface (`LlmDriver`) for interacting with
//! various remote model providers (OpenAI, Anthropic, Gemini, etc.).

use autonoetic_types::agent::LlmConfig;
use serde::{Deserialize, Serialize};
use std::error::Error as _;
use std::sync::Arc;

const LLM_BASE_URL_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_API_KEY";
const ALLOW_LLM_ENV_OVERRIDES_ENV: &str = "AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES";

pub mod anthropic;
pub mod egress_chokepoint;
pub mod gemini;
pub mod openai;
pub mod provider;
pub mod xml_tool_calls;

/// Build a `reqwest::Client` tuned for LLM API calls.
///
/// - `connect_timeout`: 15 s — fail fast on unreachable endpoints instead of
///   waiting for the OS TCP timeout (~2 min on Linux).
/// - `pool_idle_timeout`: 30 s — evict stale connections before the server does,
///   avoiding "connection reset" errors on reused pooled connections.
/// - `pool_max_idle_per_host`: 4 — cap idle connection accumulation when many
///   concurrent sessions share the same client.
/// - `tcp_keepalive`: 30 s — detect dead TCP connections proactively.
/// - **No global request timeout** — LLM streams can run for minutes; a blanket
///   `timeout()` would kill legitimate long-running responses. Instead, each
///   non-streaming `complete()` call applies a per-request timeout resolved at
///   driver-build time (env override → preset `request_timeout_secs` →
///   `llm_request_timeout_secs` → default 120s, #1045) with a fail-fast,
///   wall-clock-bounded retry policy ([`next_connection_retry_wait`]).
pub fn build_llm_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .pool_idle_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(4)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Whether a reqwest error is transient and worth retrying.
///
/// Catches: connection refused, connection reset, connection aborted,
/// timed out (connect or request), name resolution failures, and the
/// generic "error sending request for url" wrapper.
pub fn is_transient_connection_error(err: &reqwest::Error) -> bool {
    if err.is_connect()
        || err.is_timeout()
        || err.is_request()
    {
        return true;
    }
    let msg = err.to_string().to_lowercase();
    msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("connection aborted")
        || msg.contains("broken pipe")
        || msg.contains("timed out")
        || msg.contains("error sending request")
}

pub const MAX_CONNECTION_RETRIES: u32 = 3;

/// A *timeout* already consumed a full per-request budget; retrying it with
/// another full-length attempt is how a degraded endpoint compounds into many
/// minutes of wasted wall-clock. So timeouts retry at most once (fast-failing
/// connection errors like refused/reset keep [`MAX_CONNECTION_RETRIES`]).
pub const MAX_TIMEOUT_RETRIES: u32 = 1;

/// Cap for transient server-error (HTTP 5xx with a response body) retries.
/// These are rarer than connection blips but can happen when an upstream
/// provider returns a malformed-structured-output 500 or an overloaded 503.
/// We keep this lower than connection retries to avoid amplifying a genuinely
/// broken upstream endpoint.
pub const MAX_5XX_RETRIES: u32 = 2;

/// Default per-request timeout for a non-streaming `complete()` call.
/// Lowered from a previous 300s: 5 minutes per attempt let a slow/overloaded
/// endpoint burn ~20 min/turn (timeout × retries) before failing. Override with
/// `llm_request_timeout_secs` in the gateway config, or
/// `AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS` for a one-off run.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 120;

/// Lowest accepted per-request timeout. Anything below this is a
/// misconfiguration rather than an intent, and falls back to the default.
const MIN_REQUEST_TIMEOUT_SECS: u64 = 5;

/// The configured per-request timeout, published once at gateway startup from
/// [`autonoetic_types::config::GatewayConfig::llm_request_timeout_secs`].
///
/// A process-wide cell rather than a threaded parameter because some
/// `LlmConfig`-producing auxiliary paths (context compression, capsule delta
/// extraction, routing classifier) resolve from presets without a
/// `GatewayConfig` in scope. It is read exactly once per driver build, in
/// [`build_driver`]'s precedence merge — drivers themselves only ever see the
/// resolved value on `ResolvedProvider::request_timeout` (#1045).
static CONFIGURED_REQUEST_TIMEOUT_SECS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Publish the configured per-request timeout. Called once during gateway
/// startup; later calls are ignored, so a running process keeps one timeout for
/// its whole life rather than changing budget mid-turn.
pub(crate) fn set_configured_request_timeout_secs(secs: Option<u64>) {
    if let Some(secs) = secs {
        let _ = CONFIGURED_REQUEST_TIMEOUT_SECS.set(secs);
    }
}

/// Pure resolution of the per-request timeout, in precedence order (#1045):
/// `AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS` (ad-hoc override) → preset-level
/// `request_timeout_secs` (carried on the resolved `LlmConfig`) → gateway
/// `llm_request_timeout_secs` → [`DEFAULT_REQUEST_TIMEOUT_SECS`]. Values below
/// [`MIN_REQUEST_TIMEOUT_SECS`] are treated as unset and fall through.
pub(crate) fn resolve_request_timeout_secs(
    env: Option<&str>,
    preset: Option<u64>,
    configured: Option<u64>,
) -> u64 {
    env.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s >= MIN_REQUEST_TIMEOUT_SECS)
        .or_else(|| preset.filter(|s| *s >= MIN_REQUEST_TIMEOUT_SECS))
        .or_else(|| configured.filter(|s| *s >= MIN_REQUEST_TIMEOUT_SECS))
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS)
}

pub fn connection_retry_backoff_ms(attempt: u32) -> u64 {
    // (attempt + 1) so the FIRST retry already waits (#1043): `attempt * 1000`
    // made the only retry a timeout ever gets fire instantly, re-sending the
    // same heavy request the moment the previous one gave up — the worst
    // possible behaviour toward a queueing provider. Same shape as the 429
    // (`(attempt + 1) * 2000`) and 5xx (`(attempt + 1) * 1500`) backoffs.
    (attempt as u64 + 1) * 1000
}

/// Total backoff budget the retry deadline must leave room for: the sum of
/// the connection backoffs across [`MAX_CONNECTION_RETRIES`].
pub(crate) fn retry_backoff_budget() -> std::time::Duration {
    let total_ms: u64 = (0..MAX_CONNECTION_RETRIES)
        .map(connection_retry_backoff_ms)
        .sum();
    std::time::Duration::from_millis(total_ms)
}

/// The wall-clock deadline bounding a `complete()` retry loop: two full
/// per-request attempts plus the backoffs between retries (#1043). The old
/// `timeout * 2` was denominated in the same unit the attempts consume —
/// mathematically exhausted by one timed-out attempt, so the backoff could
/// never fit and the retry degenerated to an instant duplicate.
pub(crate) fn retry_deadline(complete_timeout: std::time::Duration) -> std::time::Duration {
    complete_timeout
        .saturating_mul(2)
        .saturating_add(retry_backoff_budget())
}

/// Backoff for transient server-error (HTTP 5xx) retries.
/// Slightly longer than connection blips: upstream is usually under load.
pub fn server_error_retry_backoff_ms(attempt: u32) -> u64 {
    (attempt as u64 + 1) * 1500
}

/// Retry decision for a transient connection error on an LLM call.
///
/// Returns `Some(wait_ms)` to retry after a backoff, or `None` to stop. It
/// fail-fasts on two axes the old "retry up to N times" logic ignored:
/// - **wall-clock deadline**: once `elapsed >= deadline` (typically 2× the
///   per-request timeout), stop — a persistently slow endpoint must not
///   multiply the timeout across retries.
/// - **timeout vs blip**: a request timeout (`is_timeout`) retries at most
///   [`MAX_TIMEOUT_RETRIES`]; fast-failing connection errors retry up to
/// Whether a reqwest error is a timeout. reqwest does not always set the
/// `is_timeout` flag, so the message is matched too — applied uniformly to
/// send()-phase and body-read-phase errors.
pub(crate) fn error_is_timeout(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.to_string().to_lowercase().contains("timed out")
}

/// Stable classification of a reqwest transport error, computed from
/// reqwest's structured flags (#1041, #1042). The LLM layer computes this
/// once and stamps it on the terminal error as a `llm_transport:<kind>`
/// token; the workflow failure classifier matches the token, so the two
/// layers stop disagreeing about the same error (the #1021 lesson: a
/// hand-maintained substring list per layer can never keep up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmTransportErrorKind {
    /// Our per-request timeout fired (the connection was established; the
    /// endpoint never answered in time).
    Timeout,
    /// Connection refused / reset / aborted / DNS — never connected.
    Connect,
    /// Malformed request construction (rare; still transport-phase).
    Request,
    /// HTTP status received but the body transfer broke.
    Body,
    /// Anything else reqwest can emit.
    Other,
}

impl LlmTransportErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LlmTransportErrorKind::Timeout => "timeout",
            LlmTransportErrorKind::Connect => "connect",
            LlmTransportErrorKind::Request => "request",
            LlmTransportErrorKind::Body => "body",
            LlmTransportErrorKind::Other => "other",
        }
    }
}

/// Classify a reqwest error from its structured flags. Timeout first:
/// `error_is_timeout` also matches the message text, which reqwest sets
/// inconsistently across send/read phases.
pub fn classify_transport_error(err: &reqwest::Error) -> LlmTransportErrorKind {
    if error_is_timeout(err) {
        LlmTransportErrorKind::Timeout
    } else if err.is_connect() {
        LlmTransportErrorKind::Connect
    } else if err.is_request() {
        LlmTransportErrorKind::Request
    } else if err.is_body() {
        LlmTransportErrorKind::Body
    } else {
        LlmTransportErrorKind::Other
    }
}

/// The full `source()` chain of a reqwest error, joined with `" <- "`.
///
/// reqwest's `Display` is the generic `error sending request for url (...)`;
/// the discriminating text (`operation timed out`, `Connection refused`)
/// lives in `source()` (#1042). An empty string means no chain beyond the
/// top-level error.
pub fn transport_error_source_chain(err: &reqwest::Error) -> String {
    let mut parts = Vec::new();
    let mut cur = err.source();
    while let Some(source) = cur {
        parts.push(source.to_string());
        cur = source.source();
    }
    parts.join(" <- ")
}

/// Log a transient transport retry with the structured discriminants
/// (#1042): the classified kind, the source chain, and the elapsed time of
/// the failed attempt — so a 120s timeout reads as a timeout in the log,
/// not a "connection error". The message names the kind: triage should not
/// have to cross-reference the `transport_kind` field to learn what failed.
pub fn log_transport_retry(
    kind: LlmTransportErrorKind,
    attempt: u32,
    wait_ms: u64,
    elapsed: std::time::Duration,
    err: &reqwest::Error,
) {
    let elapsed_ms = elapsed.as_millis() as u64;
    let source_chain = transport_error_source_chain(err);
    match kind {
        LlmTransportErrorKind::Timeout => tracing::warn!(
            attempt,
            wait_ms,
            elapsed_ms,
            transport_kind = kind.as_str(),
            error = %err,
            error_source_chain = %source_chain,
            "LLM request timed out, retrying"
        ),
        LlmTransportErrorKind::Connect => tracing::warn!(
            attempt,
            wait_ms,
            elapsed_ms,
            transport_kind = kind.as_str(),
            error = %err,
            error_source_chain = %source_chain,
            "LLM connection error, retrying"
        ),
        LlmTransportErrorKind::Request => tracing::warn!(
            attempt,
            wait_ms,
            elapsed_ms,
            transport_kind = kind.as_str(),
            error = %err,
            error_source_chain = %source_chain,
            "LLM request build error, retrying"
        ),
        LlmTransportErrorKind::Body => tracing::warn!(
            attempt,
            wait_ms,
            elapsed_ms,
            transport_kind = kind.as_str(),
            error = %err,
            error_source_chain = %source_chain,
            "LLM response body read failed, retrying"
        ),
        LlmTransportErrorKind::Other => tracing::warn!(
            attempt,
            wait_ms,
            elapsed_ms,
            transport_kind = kind.as_str(),
            error = %err,
            error_source_chain = %source_chain,
            "LLM transport error, retrying"
        ),
    }
}

/// The terminal error returned when a transport retry loop gives up.
///
/// Carries the stable `llm_transport:<kind>` token that the workflow failure
/// classifier (`classify_task_status`) matches — the structured hand-off
/// between the LLM layer (which computed the kind) and the workflow layer
/// (which decides retryability). Timeout → `FailureClass::Timeout`, all
/// other kinds → `FailureClass::TransientInfra`; both retryable.
pub fn transport_terminal_error(
    kind: LlmTransportErrorKind,
    attempts: u32,
    elapsed: std::time::Duration,
    err: &reqwest::Error,
) -> anyhow::Error {
    anyhow::anyhow!(
        "llm_transport:{} attempts={} elapsed_ms={} source_chain=[{}]: {}",
        kind.as_str(),
        attempts,
        elapsed.as_millis(),
        transport_error_source_chain(err),
        err,
    )
}

///   [`MAX_CONNECTION_RETRIES`].
pub fn next_connection_retry_wait(
    err: &reqwest::Error,
    attempt: u32,
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> Option<u64> {
    retry_wait_decision(
        is_transient_connection_error(err),
        error_is_timeout(err),
        attempt,
        elapsed,
        deadline,
    )
}

/// Retry decision for a response-body read failure after a successful HTTP
/// status: connection dropped mid-body, truncated chunked transfer, or a
/// decode error. These are treated as transient delivery failures — the
/// request was accepted and processed, only the body transfer broke — so
/// they retry within the provider under the same caps and wall-clock
/// deadline as connection errors. Deliberately NOT wired into
/// [`is_failover_eligible_error`]: the provider already consumed the request
/// (and may have billed it), so cross-provider failover on a delivery blip
/// risks paying twice.
pub(crate) fn next_body_read_retry_wait(
    is_timeout: bool,
    attempt: u32,
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> Option<u64> {
    retry_wait_decision(true, is_timeout, attempt, elapsed, deadline)
}

/// Pure decision core of [`next_connection_retry_wait`], split out so it can be
/// unit-tested without fabricating a `reqwest::Error`.
pub(crate) fn retry_wait_decision(
    is_transient: bool,
    is_timeout: bool,
    attempt: u32,
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> Option<u64> {
    if !is_transient {
        return None;
    }
    if elapsed >= deadline {
        return None;
    }
    let cap = if is_timeout {
        MAX_TIMEOUT_RETRIES
    } else {
        MAX_CONNECTION_RETRIES
    };
    if attempt >= cap {
        return None;
    }
    let wait_ms = connection_retry_backoff_ms(attempt);
    // Don't sleep past the deadline: if the backoff itself would push us to/over
    // the cap, stop now rather than sleep and then start an attempt that's
    // already late. (Keeps the cumulative wall-clock honest.)
    if elapsed.saturating_add(std::time::Duration::from_millis(wait_ms)) >= deadline {
        return None;
    }
    Some(wait_ms)
}

/// Whether a non-success HTTP status/body indicates a transient server error
/// worth retrying.  Context-overflow errors are handled separately and must NOT
/// be routed here.
pub fn is_transient_server_error(status: u16, body: &str) -> bool {
    if !matches!(status, 500 | 502 | 503 | 504) {
        return false;
    }
    // Trim first: some providers return whitespace/newline-only bodies on 5xx,
    // which should be treated the same as an empty (transient) body.
    let lc = body.trim().to_lowercase();
    if lc.is_empty() {
        return true;
    }
    const TRANSIENT_PHRASES: &[&str] = &[
        "overloaded",
        "temporarily unavailable",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "peg-native",
        "server_error",
        "try again",
        "try again later",
    ];
    TRANSIENT_PHRASES.iter().any(|phrase| lc.contains(phrase))
}

/// RFC #779 Part E.2 — classify whether an error that escaped within-provider
/// retry is eligible for **cross-provider failover**.
///
/// Only genuinely transient errors justify trying a different provider/model:
/// a 400 (bad request), 401 (auth), or 403 (forbidden) is deterministic —
/// the same request to a different provider will likely fail differently, not
/// succeed. The within-provider retry already handled connection blips and
/// transient 5xx; if the error reaches here, the provider is genuinely down
/// or rate-limiting hard.
///
/// Eligible conditions:
/// - HTTP 429 (rate limit / too many requests)
/// - HTTP 5xx (server error) — already retried within provider, now try elsewhere
/// - Connection-level failures (refused, reset, timeout)
/// - Provider-specific "overloaded" signals (529, overloaded)
///
/// NOT eligible (deterministic errors):
/// - 400/401/403 — bad request, auth failure, forbidden
/// - Context overflow (handled separately by the context governor)
/// - Validation / schema errors
pub fn is_failover_eligible_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();

    // Context overflow is handled separately by the context governor (P-6.9),
    // not by provider failover — even if it arrives with a 5xx status.
    if msg.contains("context_overflow")
        || msg.contains("context window")
        || msg.contains("context_length_exceeded")
        || msg.contains("max_context_window_reached")
        || msg.contains("resource_exhausted")
    {
        return false;
    }

    // Rate limiting
    if msg.contains("429")
        || msg.contains("rate limit")
        || msg.contains("rate_limit")
        || msg.contains("too many requests")
    {
        return true;
    }

    // Transient server errors (5xx) — already retried within provider
    if msg.contains("500")
        || msg.contains("502")
        || msg.contains("503")
        || msg.contains("504")
        || msg.contains("529") // Anthropic overloaded
        || msg.contains("overloaded")
        || msg.contains("internal server error")
        || msg.contains("bad gateway")
        || msg.contains("service unavailable")
        || msg.contains("gateway timeout")
        || msg.contains("server_error")
        || msg.contains("temporarily unavailable")
    {
        return true;
    }

    // Connection-level failures
    if msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("connection aborted")
        || msg.contains("broken pipe")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("error sending request")
        || msg.contains("dns error")
        || msg.contains("name resolution")
    {
        return true;
    }

    false
}

/// Retry decision for a transient server-error (HTTP 5xx) response.
///
/// Returns `Some(wait_ms)` to retry after a backoff, or `None` to stop. Uses the
/// same wall-clock deadline discipline as [`next_connection_retry_wait`] but with
/// a separate retry cap ([`MAX_5XX_RETRIES`]) and a longer backoff.
pub fn next_server_error_retry_wait(
    status: u16,
    body: &str,
    attempt: u32,
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> Option<u64> {
    if !is_transient_server_error(status, body) {
        return None;
    }
    if elapsed >= deadline {
        return None;
    }
    if attempt >= MAX_5XX_RETRIES {
        return None;
    }
    let wait_ms = server_error_retry_backoff_ms(attempt);
    if elapsed.saturating_add(std::time::Duration::from_millis(wait_ms)) >= deadline {
        return None;
    }
    Some(wait_ms)
}

/// Check whether a non-success status / body indicates a context-overflow error
/// for any of the supported providers.  Returns `true` when the governor should
/// trigger reduction strategies rather than propagate a fatal error.
///
/// Pattern matching order:
///  1. `error.code == "context_length_exceeded"` — OpenAI cloud (string code)
///  2. `error.type == "exceed_context_size_error"` — llama.cpp / lmstudio /
///     other OpenAI-compatible local servers (numeric code, structured `type`)
///  3. `error.message` containing context-overflow phrasing — generic fallback
///     for OpenAI-compatible servers that don't standardize the `type` field
///  4. `n_prompt_tokens` + `n_ctx` keys present — llama.cpp structured signal
///  5. `max_context_window_reached` — Anthropic
///  6. `RESOURCE_EXHAUSTED` + `context` — Gemini
///  7. `SAFETY` — content-filter rejections (not overflow but surfaced the same way)
pub fn is_context_overflow_error(status: u16, body: &str) -> bool {
    if status == 0 {
        return false;
    }

    // OpenRouter / OpenAI / llama.cpp / lmstudio — parse the JSON error body
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(err) = val.get("error") {
            // 1. OpenAI cloud: error.code == "context_length_exceeded"
            if let Some(code) = err.get("code").and_then(|c| c.as_str()) {
                if code == "context_length_exceeded" {
                    return true;
                }
            }
            // 2. llama.cpp / lmstudio: error.type == "exceed_context_size_error"
            if let Some(err_type) = err.get("type").and_then(|t| t.as_str()) {
                if err_type == "exceed_context_size_error"
                    || err_type == "context_length_exceeded"
                {
                    return true;
                }
            }
            // 3. Generic message-text signal (case-insensitive) for any
            //    OpenAI-compatible server that doesn't use a standard code/type.
            if let Some(msg) = err.get("message").and_then(|m| m.as_str()) {
                let lc = msg.to_lowercase();
                let has_overflow_verb = lc.contains("exceed")
                    || lc.contains("too long")
                    || lc.contains("too many tokens");
                // A size/length/window/token hint distinguishes a *context size*
                // overflow from unrelated "context" errors that merely share the
                // word — most importantly "context deadline exceeded" (a timeout,
                // NOT an overflow), which must not be routed into overflow recovery.
                let has_size_hint = lc.contains("size")
                    || lc.contains("length")
                    || lc.contains("window")
                    || lc.contains("token");
                if lc.contains("exceeds the available context")
                    || lc.contains("exceeds the context")
                    || lc.contains("context length exceeded")
                    || lc.contains("maximum context length")
                    || (lc.contains("context window") && has_overflow_verb)
                    // General catch for OpenAI-compatible servers that phrase it
                    // differently (e.g. llama.cpp / vLLM "Context size has been
                    // exceeded."). Requires "context" + an overflow verb + a
                    // size/length/window/token hint, so timeouts like
                    // "context deadline exceeded" are NOT misclassified.
                    || (lc.contains("context") && has_overflow_verb && has_size_hint)
                {
                    return true;
                }
            }
            // 4. llama.cpp structured signal: presence of n_prompt_tokens / n_ctx
            //    in the error body means the server explicitly reported a
            //    context-size violation, regardless of the message wording.
            if err.get("n_prompt_tokens").is_some() && err.get("n_ctx").is_some() {
                return true;
            }
        }
    }

    // Anthropic — full text search (Anthropic doesn't always JSON-encode)
    if body.contains("max_context_window_reached") {
        return true;
    }

    // Gemini RESOURCE_EXHAUSTED context overflow
    if body.contains("RESOURCE_EXHAUSTED") && body.contains("context") {
        return true;
    }

    // Safety filter — treat as overflow-like so the governor handles it
    if body.contains("SAFETY") {
        return true;
    }

    false
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// A conversation message role.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    /// Used for tool-result turns sent back to the model.
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

// ---------------------------------------------------------------------------
// Tool types
// ---------------------------------------------------------------------------

/// A tool the agent can invoke. Sent to the LLM so it knows what's available.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's parameters (as a raw serde_json value).
    pub input_schema: serde_json::Value,
}

/// A tool invocation requested by the model in a response.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// Opaque identifier used to match the result back to this call.
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments string (matches what the model returns).
    pub arguments: String,
}

/// A tool result sent back to the model after the agent executes a tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    /// The ID of the ToolCall this is a result for.
    pub tool_call_id: String,
    /// The name of the tool (needed by Anthropic's API for routing).
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A single message in a conversation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    /// Stable per-message id (`msg_<id>`), minted at history-commit time
    /// (RFC data-envelopes §3.4). It is the join key between an assistant /
    /// user / synthesized message and its egress label in the session sidecar
    /// (tool-result messages join by `tool_call_id` instead). `None` for
    /// transient / uncommitted messages and for history predating this field;
    /// never sent to a provider (drivers translate `Message` to their own wire
    /// shape and ignore it), and `#[serde(default, skip_if_none)]` so it is
    /// omitted from checkpoints when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: Role,
    pub content: String,
    /// Optional tool calls from an assistant turn.
    pub tool_calls: Vec<ToolCall>,
    /// For Role::Tool turns, the matching call ID.
    pub tool_call_id: Option<String>,
    /// Provider-specific assistant reasoning content that must be replayed on
    /// subsequent turns for some thinking/reasoning models (DeepSeek-direct
    /// `reasoning_content`, or flattened text from OpenRouter `reasoning`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Raw structured reasoning blocks (OpenRouter `reasoning_details` array).
    /// Preserved verbatim so signed/encrypted reasoning round-trips correctly
    /// across tool-call turns; replayed in preference to `reasoning_content`
    /// when the provider expects the structured form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<serde_json::Value>,
}

impl Message {
    /// Convenience: plain text user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            id: None,
            role: Role::User,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    /// Convenience: system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            id: None,
            role: Role::System,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    /// Convenience: assistant text message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            id: None,
            role: Role::Assistant,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: None,
            reasoning_content: None,
            reasoning_details: None,
        }
    }

    /// Convenience: tool result message.
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        let _ = tool_name; // stored via ToolResult struct; kept in signature for API clarity
        Self {
            id: None,
            role: Role::Tool,
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
            reasoning_details: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

/// A request to an LLM provider.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletionRequest {
    /// Model identifier (e.g., "gpt-4o", "claude-3-5-sonnet-20241022").
    pub model: String,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Tool definitions available to the model.
    pub tools: Vec<ToolDefinition>,
    /// Maximum tokens to generate (optional).
    pub max_tokens: Option<u32>,
    /// Sampling temperature (optional).
    pub temperature: Option<f32>,
    /// Optional metadata for pipeline hooks (e.g. skip_llm, assistant_reply).
    pub metadata: Option<std::collections::HashMap<String, serde_json::Value>>,
    /// Extended thinking configuration. When set, the driver translates this
    /// to provider-native format (e.g., reasoning_effort for OpenAI o-series,
    /// thinking budget for Anthropic, <|think|> token for Gemma).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<autonoetic_types::agent::ThinkingConfig>,
    /// Stable cache key for provider prompt caching (OpenRouter/OpenAI
    /// `prompt_cache_key`). Typically the session id, so repeated turns in a
    /// session reuse cached prompt-prefix tokens. Drivers emit it only for
    /// providers that support it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Byte length of the leading portion of the (single) system message that
    /// is STABLE across turns and therefore safe to mark as a provider cache
    /// prefix. The volatile suffix (state attestation, degradation notice,
    /// per-turn memory context) follows this boundary. `None` disables the
    /// cache breakpoint. Cache-capable drivers (Anthropic; OpenRouter routing
    /// Claude/Gemini) split the system content here and attach
    /// `cache_control: {type: ephemeral}` to the prefix block; other providers
    /// ignore it (llama.cpp/OpenAI reuse a stable prefix automatically).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_cache_prefix_bytes: Option<usize>,
}

impl CompletionRequest {
    pub fn simple(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: None,
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        }
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Other(String),
}

/// Token usage statistics returned by the provider.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Reasoning tokens, a subset of `output_tokens`, billed as output but
    /// spent on hidden chain-of-thought. From `completion_tokens_details
    /// .reasoning_tokens` (OpenAI/OpenRouter). 0 when unknown.
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// Prompt tokens served from the provider's cache, a subset of
    /// `input_tokens`. From `prompt_tokens_details.cached_tokens`. 0 when
    /// unknown. Useful for cost attribution when prompt caching is enabled.
    #[serde(default)]
    pub cached_tokens: u64,
}

/// Full response from a completion call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletionResponse {
    /// Text content (may be empty if the model only returned tool calls).
    pub text: String,
    /// Tool calls requested by the model (may be empty).
    pub tool_calls: Vec<ToolCall>,
    /// Provider-specific assistant reasoning content that should be replayed
    /// with the assistant turn when required by the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Raw structured reasoning blocks (OpenRouter `reasoning_details`),
    /// preserved verbatim for round-trip on the next assistant turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<serde_json::Value>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

impl CompletionResponse {
    pub fn text_only(text: String) -> Self {
        Self {
            text,
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// Events emitted during SSE streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolInputDelta(String),
    ToolUseEnd {
        id: String,
        name: String,
        arguments: String,
    },
    Complete {
        stop_reason: StopReason,
        usage: TokenUsage,
    },
}

// ---------------------------------------------------------------------------
// LlmDriver trait
// ---------------------------------------------------------------------------

/// The unified LLM driver interface.
#[async_trait::async_trait]
pub trait LlmDriver: Send + Sync {
    /// Send a completion request and receive a full structured response.
    async fn complete(&self, request: &CompletionRequest) -> anyhow::Result<CompletionResponse>;

    /// Stream a completion, sending incremental events to the channel.
    /// Default implementation wraps `complete()` with a single text chunk.
    async fn stream(
        &self,
        request: &CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<CompletionResponse> {
        let response = self.complete(request).await?;
        if !response.text.is_empty() {
            let _ = tx.send(StreamEvent::TextDelta(response.text.clone())).await;
        }
        let _ = tx
            .send(StreamEvent::Complete {
                stop_reason: response.stop_reason.clone(),
                usage: response.usage.clone(),
            })
            .await;
        Ok(response)
    }

    /// The resolved per-request timeout for this driver (#1045). On the
    /// streaming turn path (#1044) the value doubles as the **idle-gap**
    /// budget: the maximum silence between streamed chunks before the turn
    /// declares the upstream stalled.
    fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS)
    }
}

/// Run a turn's completion through the streaming path with stall detection
/// (#1044).
///
/// A blocking `complete()` cannot distinguish "the upstream accepted and
/// stalled without emitting anything" from "generation was underway and the
/// wall-clock ceiling cut it off" — both surface as a bare timeout after zero
/// observable output, and they need opposite fixes (fail fast vs. raise the
/// budget). Streaming makes time-to-first-byte a first-class signal and
/// replaces the whole-response wall-clock cap with an **idle-gap** timeout:
/// silence longer than `driver.request_timeout()` between chunks declares a
/// stall. A legitimately long generation that keeps emitting is no longer
/// punished for its length.
///
/// Providers without real streaming fall back to the trait's default
/// `stream()` (a single chunk around `complete()`), where the idle gap
/// degenerates to the same whole-request cap as before — no regression.
///
/// A stall aborts the in-flight attempt and returns an error carrying the
/// `llm_transport:timeout` token (#1041), so the workflow classifier marks it
/// retryable and the failover chain can try the next preset. Nothing is
/// salvaged from a stalled stream: partial tool-call JSON is unsafe to
/// append to history, and the turn-level retry starts clean.
pub async fn complete_with_stall_detection(
    driver: &Arc<dyn LlmDriver>,
    req: &CompletionRequest,
) -> anyhow::Result<CompletionResponse> {
    let idle = driver.request_timeout();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(256);
    let task_driver = driver.clone();
    let task_req = req.clone();
    let mut task = tokio::spawn(async move { task_driver.stream(&task_req, tx).await });

    let start = std::time::Instant::now();
    let mut ttfb: Option<std::time::Duration> = None;
    let mut chunks: u64 = 0;
    let mut text_chars: u64 = 0;
    loop {
        match tokio::time::timeout(idle, rx.recv()).await {
            Ok(Some(event)) => {
                if ttfb.is_none() {
                    ttfb = Some(start.elapsed());
                }
                chunks += 1;
                if let StreamEvent::TextDelta(t) = &event {
                    text_chars += t.len() as u64;
                }
            }
            Ok(None) => {
                // Channel closed: the stream task finished. Join for the result.
                let elapsed_ms = start.elapsed().as_millis() as u64;
                match task.await {
                    Ok(result) => {
                        match &result {
                            Ok(_) => tracing::debug!(
                                target: "llm",
                                ttfb_ms = ttfb.map(|d| d.as_millis() as u64),
                                chunks,
                                text_chars,
                                elapsed_ms,
                                "LLM stream completed"
                            ),
                            Err(_) => tracing::debug!(
                                target: "llm",
                                ttfb_ms = ttfb.map(|d| d.as_millis() as u64),
                                chunks,
                                elapsed_ms,
                                "LLM stream failed"
                            ),
                        }
                        return result;
                    }
                    Err(join_err) => {
                        return Err(anyhow::anyhow!(
                            "llm_transport:other attempts=1 elapsed_ms={} source_chain=[]: \
                             stream task join failed: {}",
                            elapsed_ms,
                            join_err
                        ));
                    }
                }
            }
            Err(_elapsed) => {
                // Idle gap exceeded: the upstream stalled. Abort the attempt;
                // the turn-level failover/retry starts clean.
                task.abort();
                let elapsed_ms = start.elapsed().as_millis() as u64;
                let phase = if ttfb.is_none() {
                    "stalled before first byte"
                } else {
                    "stalled mid-stream"
                };
                tracing::warn!(
                    target: "llm",
                    phase,
                    idle_ms = idle.as_millis() as u64,
                    ttfb_ms = ttfb.map(|d| d.as_millis() as u64),
                    chunks,
                    text_chars,
                    elapsed_ms,
                    "LLM stream stalled"
                );
                return Err(anyhow::anyhow!(
                    "llm_transport:timeout attempts=1 elapsed_ms={} source_chain=[]: \
                     stream {} — no chunk for {}ms (chunks={}, text_chars={}, ttfb_ms={:?})",
                    elapsed_ms,
                    phase,
                    idle.as_millis(),
                    chunks,
                    text_chars,
                    ttfb.map(|d| d.as_millis()),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Build the appropriate driver for the given config.
///
/// Credential/endpoint resolution is centralised in `provider::resolve()` —
/// drivers themselves never read environment variables.
pub fn build_driver(
    config: LlmConfig,
    client: reqwest::Client,
) -> anyhow::Result<Arc<dyn LlmDriver>> {
    let allow_env_overrides = llm_env_overrides_allowed();
    let base_url_override = if allow_env_overrides {
        std::env::var(LLM_BASE_URL_OVERRIDE_ENV)
            .ok()
            .or(config.base_url.clone())
    } else {
        if std::env::var(LLM_BASE_URL_OVERRIDE_ENV).ok().is_some() {
            tracing::warn!(
                env = LLM_BASE_URL_OVERRIDE_ENV,
                gate = ALLOW_LLM_ENV_OVERRIDES_ENV,
                "Ignoring LLM base URL env override in strict mode"
            );
        }
        config.base_url.clone()
    };
    let api_key_override = if allow_env_overrides {
        std::env::var(LLM_API_KEY_OVERRIDE_ENV).ok().or_else(|| {
            // If the preset specifies a custom env var name, read from that.
            // This lets OpenAI-compatible providers (StreamLake, etc.) use their
            // own key env var instead of the provider's default (e.g., OPENAI_API_KEY).
            config
                .api_key_env
                .as_ref()
                .and_then(|env_name| std::env::var(env_name).ok())
        })
    } else {
        if std::env::var(LLM_API_KEY_OVERRIDE_ENV).ok().is_some() {
            tracing::warn!(
                env = LLM_API_KEY_OVERRIDE_ENV,
                gate = ALLOW_LLM_ENV_OVERRIDES_ENV,
                "Ignoring LLM API key env override in strict mode"
            );
        }
        config
            .api_key_env
            .as_ref()
            .and_then(|env_name| std::env::var(env_name).ok())
    };
    // Resolve the per-request timeout at the factory (#1045): env override →
    // preset field (carried on the merged LlmConfig) → gateway default
    // (published at config load) → built-in default. Drivers read the value
    // off `ResolvedProvider`; they never read the global themselves.
    let request_timeout = std::time::Duration::from_secs(resolve_request_timeout_secs(
        std::env::var("AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS")
            .ok()
            .as_deref(),
        config.request_timeout_secs,
        CONFIGURED_REQUEST_TIMEOUT_SECS.get().copied(),
    ));
    let resolved = provider::resolve(
        &config.provider,
        &config.model,
        if config.temperature > 0.0 {
            Some(config.temperature as f32)
        } else {
            None
        },
        None, // max_tokens from request, not config
        base_url_override.as_deref(),
        api_key_override.as_deref(),
        config.chat_only,
        config.egress_class,
        request_timeout,
    )?;

    // Capture the egress sink before `resolved` is moved into the inner driver.
    let egress_sink = resolved.egress_class.as_sink();
    let inner: Arc<dyn LlmDriver> = match resolved.kind {
        provider::DriverKind::Anthropic => {
            Arc::new(anthropic::AnthropicDriver::new(client, resolved))
        }
        provider::DriverKind::Gemini => Arc::new(gemini::GeminiDriver::new(client, resolved)),
        provider::DriverKind::OpenAi => Arc::new(openai::OpenAiDriver::new(client, resolved)),
    };
    // Wrap in the egress chokepoint (RFC data-envelopes §5.2). The wrapper is a
    // zero-cost pass-through when no label map is attached to a request's
    // metadata (the common case — unconfigured deployments, or auxiliary LLM
    // calls like capsule/digest that don't carry labels). Wrapping here covers
    // the primary completion AND every failover preset uniformly, closing the
    // local→remote failover leak for free.
    let driver = Arc::new(egress_chokepoint::EgressChokepointDriver::new(
        inner,
        egress_sink,
    )) as Arc<dyn LlmDriver>;
    Ok(driver)
}

fn llm_env_overrides_allowed() -> bool {
    std::env::var(ALLOW_LLM_ENV_OVERRIDES_ENV)
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}
