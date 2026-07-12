//! LLM Driver Abstraction and Types.
//!
//! Provides a thin, unified interface (`LlmDriver`) for interacting with
//! various remote model providers (OpenAI, Anthropic, Gemini, etc.).

use autonoetic_types::agent::LlmConfig;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const LLM_BASE_URL_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_BASE_URL";
const LLM_API_KEY_OVERRIDE_ENV: &str = "AUTONOETIC_LLM_API_KEY";
const ALLOW_LLM_ENV_OVERRIDES_ENV: &str = "AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES";

pub mod anthropic;
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
///   non-streaming `complete()` call applies a per-request timeout
///   ([`request_timeout`], default 120s, env `AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS`)
///   with a fail-fast, wall-clock-bounded retry policy
///   ([`next_connection_retry_wait`]).
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
/// `AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS`.
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 120;

/// The per-request completion timeout, from `AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS`
/// (clamped to a sane floor) or [`DEFAULT_REQUEST_TIMEOUT_SECS`].
pub fn request_timeout() -> std::time::Duration {
    let secs = std::env::var("AUTONOETIC_LLM_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s >= 5)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
}

pub fn connection_retry_backoff_ms(attempt: u32) -> u64 {
    (attempt as u64) * 1000
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
///   [`MAX_CONNECTION_RETRIES`].
pub fn next_connection_retry_wait(
    err: &reqwest::Error,
    attempt: u32,
    elapsed: std::time::Duration,
    deadline: std::time::Duration,
) -> Option<u64> {
    let is_timeout = err.is_timeout() || err.to_string().to_lowercase().contains("timed out");
    retry_wait_decision(
        is_transient_connection_error(err),
        is_timeout,
        attempt,
        elapsed,
        deadline,
    )
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
    )?;

    let driver: Arc<dyn LlmDriver> = match resolved.kind {
        provider::DriverKind::Anthropic => {
            Arc::new(anthropic::AnthropicDriver::new(client, resolved))
        }
        provider::DriverKind::Gemini => Arc::new(gemini::GeminiDriver::new(client, resolved)),
        provider::DriverKind::OpenAi => Arc::new(openai::OpenAiDriver::new(client, resolved)),
    };
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
