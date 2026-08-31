//! OpenAI-compatible LLM Driver.
//!
//! Handles OpenAI, OpenRouter, Groq, Together, DeepSeek, Mistral, Ollama, etc.
//! All routing decisions (base URL, auth headers, capabilities) are resolved
//! externally by `provider::resolve()` before this driver is instantiated.

use super::{
    CompletionRequest, CompletionResponse, LlmDriver, Role, StopReason, StreamEvent, TokenUsage,
    ToolCall,
};
use crate::llm::provider::{AuthStrategy, ResolvedProvider};
use reqwest::Client;
use serde_json::json;

pub struct OpenAiDriver {
    client: Client,
    provider: ResolvedProvider,
}

/// Whether a model uses `max_completion_tokens` instead of `max_tokens`.
/// (GPT-5 and o-series reasoning models require this.)
fn uses_completion_tokens(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("gpt-5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

/// Check if a model is known to NOT support tool/function calling.
/// Some multimodal or specialized models only support text completion.
fn model_supports_tools(model: &str) -> bool {
    let m = model.to_lowercase();
    if m.contains("healer-alpha") || m.contains("healer_alpha") {
        return false;
    }
    true
}

/// Moonshot's schema validator rejects a `type` field sitting alongside `anyOf`
/// or `oneOf` at the same level. Move the parent `type` into each branch item
/// and drop it from the parent.
pub(crate) fn sanitize_schema_for_strict_anyof(schema: &serde_json::Value) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            let typ = map.get("type").cloned();
            let has_branches = map.contains_key("anyOf") || map.contains_key("oneOf");
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if k == "anyOf" || k == "oneOf" {
                    if let serde_json::Value::Array(branches) = v {
                        let new_branches: Vec<serde_json::Value> = branches
                            .iter()
                            .map(|branch| {
                                let mut branch = sanitize_schema_for_strict_anyof(branch);
                                if has_branches && branch.get("type").is_none() {
                                    if let Some(ref t) = typ {
                                        if let serde_json::Value::Object(ref mut b) = branch {
                                            b.insert("type".to_string(), t.clone());
                                        }
                                    }
                                }
                                branch
                            })
                            .collect();
                        out.insert(k.clone(), serde_json::Value::Array(new_branches));
                    } else {
                        out.insert(k.clone(), sanitize_schema_for_strict_anyof(v));
                    }
                } else {
                    out.insert(k.clone(), sanitize_schema_for_strict_anyof(v));
                }
            }
            if has_branches && out.contains_key("type") {
                out.remove("type");
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter().map(sanitize_schema_for_strict_anyof).collect(),
        ),
        other => other.clone(),
    }
}

/// OpenCode Go's gateway accepts Anthropic-style `cache_control` breakpoints on
/// most models, but passes them through untouched to GLM/Zhipu upstreams, which
/// reject the extra field. Match the model ids Pi's extension skips.
fn model_is_opencode_cache_unsupported(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("glm") || m.contains("zhipu")
}

/// Whether an OpenRouter model id routes to a provider that honors Anthropic-style
/// `cache_control` breakpoints (Claude, Gemini). OpenRouter ids are namespaced
/// (`anthropic/claude-…`, `google/gemini-…`); we also match bare family names.
fn model_supports_openrouter_cache_control(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("anthropic/")
        || m.starts_with("google/")
        || m.contains("claude")
        || m.contains("gemini")
}

/// Build an OpenRouter system-message `content` array that marks the stable
/// leading `prefix_bytes` with `cache_control: {type: ephemeral}` and leaves the
/// volatile suffix uncached. Falls back to the plain string when no valid
/// boundary is supplied.
fn openrouter_cached_system_content(
    content: &str,
    prefix_bytes: Option<usize>,
) -> serde_json::Value {
    let ephemeral = || json!({ "type": "ephemeral" });
    match prefix_bytes {
        Some(n) if n >= content.len() && !content.trim().is_empty() => {
            json!([{ "type": "text", "text": content, "cache_control": ephemeral() }])
        }
        Some(n) if n > 0 && n < content.len() && content.is_char_boundary(n) => {
            let (prefix, suffix) = content.split_at(n);
            let mut parts = vec![json!({
                "type": "text",
                "text": prefix,
                "cache_control": ephemeral(),
            })];
            if !suffix.trim().is_empty() {
                parts.push(json!({ "type": "text", "text": suffix }));
            }
            json!(parts)
        }
        _ => json!(content),
    }
}

/// Anthropic-style `cache_control` used by the OpenCode Go gateway. The `ttl`
/// is the documented maximum for this control; combined with top-level
/// `prompt_cache_retention: "24h"` it keeps long sessions cheap across pauses.
fn opencode_cache_control() -> serde_json::Value {
    json!({ "type": "ephemeral", "ttl": "1h" })
}

/// Wrap a plain text message in a single content block marked with the OpenCode
/// Go `cache_control` breakpoint.
fn opencode_cached_text_content(content: &str) -> serde_json::Value {
    json!([{
        "type": "text",
        "text": content,
        "cache_control": opencode_cache_control(),
    }])
}

/// Build an OpenCode Go system-message `content` array. The stable leading
/// `prefix_bytes` is cached; any volatile suffix is appended uncached. When no
/// boundary is supplied the whole system message is cached.
fn opencode_cached_system_content(
    content: &str,
    prefix_bytes: Option<usize>,
) -> serde_json::Value {
    match prefix_bytes {
        Some(n) if n >= content.len() && !content.trim().is_empty() => {
            opencode_cached_text_content(content)
        }
        Some(n) if n > 0 && n < content.len() && content.is_char_boundary(n) => {
            let (prefix, suffix) = content.split_at(n);
            let mut parts = vec![json!({
                "type": "text",
                "text": prefix,
                "cache_control": opencode_cache_control(),
            })];
            if !suffix.trim().is_empty() {
                parts.push(json!({ "type": "text", "text": suffix }));
            }
            json!(parts)
        }
        _ => {
            if content.trim().is_empty() {
                json!(content)
            } else {
                opencode_cached_text_content(content)
            }
        }
    }
}

fn model_is_reasoning_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.contains("-o1-")
        || m.contains("-o3-")
}

impl OpenAiDriver {
    pub fn new(client: Client, provider: ResolvedProvider) -> Self {
        Self { client, provider }
    }

    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut b = builder;
        match &self.provider.auth {
            AuthStrategy::BearerToken(key) => {
                b = b.header("Authorization", format!("Bearer {}", key));
            }
            AuthStrategy::None => {}
            _ => {} // unreachable for OpenAI, but handled gracefully
        }
        for (k, v) in &self.provider.extra_headers {
            b = b.header(k, v);
        }
        b
    }

    fn build_body(&self, req: &CompletionRequest, stream: bool) -> serde_json::Value {
        // OpenRouter passes `cache_control` through to Anthropic/Gemini models
        // (OpenAI-family models cache automatically and ignore it). Only emit it
        // for OpenRouter + a Claude/Gemini model with a stable prefix boundary,
        // so strict OpenAI-compatible providers never see the extra field.
        let cache_system = req.system_cache_prefix_bytes.is_some()
            && matches!(
                self.provider.capabilities.reasoning,
                crate::llm::provider::ReasoningStyle::OpenRouterUnified
            )
            && model_supports_openrouter_cache_control(&self.provider.model);

        // OpenCode Go honors Anthropic-style `cache_control` breakpoints on all
        // models except GLM/Zhipu. We mark the stable system prefix, the last two
        // user/assistant messages, and the last tool definition.
        let is_opencode = matches!(
            self.provider.capabilities.reasoning,
            crate::llm::provider::ReasoningStyle::OpenCodeGo
        );
        let opencode_cache_supported =
            is_opencode && !model_is_opencode_cache_unsupported(&self.provider.model);

        // Indices of the last two non-tool conversation turns; these move every
        // turn but keep the recent context cached so the growing tail doesn't
        // invalidate the whole prefix.
        let mut user_assistant_indices: Vec<usize> = req
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User || m.role == Role::Assistant)
            .map(|(i, _)| i)
            .collect();
        let last_two_ua: std::collections::HashSet<usize> = user_assistant_indices
            .split_off(user_assistant_indices.len().saturating_sub(2))
            .into_iter()
            .collect();

        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .enumerate()
            .map(|(idx, m)| {
                let mut msg = json!({ "role": m.role.as_str() });

                if !m.content.is_empty() {
                    if m.role == Role::System && cache_system {
                        msg["content"] = openrouter_cached_system_content(
                            &m.content,
                            req.system_cache_prefix_bytes,
                        );
                    } else if m.role == Role::System && opencode_cache_supported {
                        msg["content"] = opencode_cached_system_content(
                            &m.content,
                            req.system_cache_prefix_bytes,
                        );
                    } else if opencode_cache_supported && last_two_ua.contains(&idx) {
                        msg["content"] = opencode_cached_text_content(&m.content);
                    } else {
                        msg["content"] = json!(m.content);
                    }
                }
                if !m.tool_calls.is_empty() {
                    msg["tool_calls"] = json!(m
                        .tool_calls
                        .iter()
                        .map(|tc| json!({
                            "id": tc.id,
                            "type": "function",
                            "function": { "name": tc.name, "arguments": tc.arguments }
                        }))
                        .collect::<Vec<_>>());
                }
                if let Some(ref id) = m.tool_call_id {
                    msg["tool_call_id"] = json!(id);
                }
                // Replay reasoning on assistant turns so reasoning models keep
                // their chain-of-thought across tool-call rounds. Prefer the
                // structured `reasoning_details` (OpenRouter, preserves signed/
                // encrypted blocks); fall back to plain `reasoning_content`
                // (DeepSeek-direct / OpenAI-compatible). Only assistant turns
                // carry reasoning — sending these fields on user/tool/system
                // messages is meaningless and some endpoints reject it.
                if m.role == Role::Assistant {
                    if let Some(ref details) = m.reasoning_details {
                        msg["reasoning_details"] = details.clone();
                    } else if let Some(ref reasoning_content) = m.reasoning_content {
                        msg["reasoning_content"] = json!(reasoning_content);
                    }
                }
                msg
            })
            .collect();

        let (token_key, token_val) = if uses_completion_tokens(&self.provider.model) {
            (
                "max_completion_tokens",
                req.max_tokens.or(self.provider.max_tokens),
            )
        } else {
            ("max_tokens", req.max_tokens.or(self.provider.max_tokens))
        };

        let mut body = json!({
            "model": self.provider.model,
            "messages": messages,
            "stream": stream,
        });

        // Ask for usage stats on the streaming path so reasoning/cache token
        // accounting works (no-op for providers that ignore the option).
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }

        if let Some(v) = token_val {
            body[token_key] = json!(v);
        }

        let t = req.temperature.or(self.provider.temperature);
        if let Some(t) = t {
            if t > 0.0 {
                // Round temperature to 2 decimal places to avoid floating-point
                // precision issues (e.g., 0.699999988079071 -> 0.7)
                // Some providers (Z.AI) reject precision values
                // Use f64 for proper rounding and JSON serialization
                let rounded = ((t as f64) * 100.0).round() / 100.0;
                body["temperature"] = json!(rounded);
            }
        }

        // Only include tools if provider supports them AND the model supports them
        let model_supports_tools = model_supports_tools(&self.provider.model);
        if !req.tools.is_empty()
            && self.provider.capabilities.supports_tools
            && model_supports_tools
        {
            // OpenRouter passes `cache_control` through to Anthropic/Gemini
            // upstreams for tool definitions too (same mechanism as the system
            // message). When the system prefix is being cached AND we're routing
            // to a cache_control-honoring model, mark the last tool so the whole
            // tool catalog is cached across turns. OpenAI-family and plain
            // providers cache automatically by prefix and must not see the field.
            let mark_tools_openrouter = cache_system;
            let mark_last_tool_opencode = opencode_cache_supported;
            let sanitize_schema = self.provider.capabilities.strict_schema_anyof;
            body["tools"] = serde_json::Value::Array(
                req.tools
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let schema = if sanitize_schema {
                            sanitize_schema_for_strict_anyof(&t.input_schema)
                        } else {
                            t.input_schema.clone()
                        };
                        let mut entry = json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": schema,
                            }
                        });
                        if mark_tools_openrouter && i == req.tools.len() - 1 {
                            entry["function"]["cache_control"] = json!({ "type": "ephemeral" });
                        }
                        if mark_last_tool_opencode && i == req.tools.len() - 1 {
                            entry["function"]["cache_control"] = opencode_cache_control();
                        }
                        entry
                    })
                    .collect(),
            );
            if self.provider.capabilities.supports_tool_choice {
                body["tool_choice"] = json!("auto");
            }
        }

        if let Some(ref thinking) = req.thinking {
            use autonoetic_types::agent::ThinkingEffort;
            match self.provider.capabilities.reasoning {
                crate::llm::provider::ReasoningStyle::None => {}
                crate::llm::provider::ReasoningStyle::OpenCodeGo => {
                    // OpenCode Go's gateway is OpenAI-compatible but does not
                    // document a provider-native reasoning field shape. Keep the
                    // request clean until a supported mapping is known.
                }
                crate::llm::provider::ReasoningStyle::OpenAiEffort => {
                    // OpenAI's `reasoning_effort` only accepts low|medium|high,
                    // so `XHigh` collapses to "high". The field is also rejected
                    // on non-reasoning models, so we gate by model name.
                    if model_is_reasoning_model(&self.provider.model) {
                        let effort_str = match thinking.effort {
                            ThinkingEffort::Low => "low",
                            ThinkingEffort::Medium => "medium",
                            ThinkingEffort::High | ThinkingEffort::XHigh => "high",
                        };
                        body["reasoning_effort"] = json!(effort_str);
                    }
                }
                crate::llm::provider::ReasoningStyle::OpenRouterUnified => {
                    // OpenRouter exposes a distinct "xhigh" tier (e.g. DeepSeek
                    // V4 Flash maps xhigh → max reasoning). Pass it through
                    // literally. OpenRouter silently ignores `reasoning` on
                    // models that don't support it, so we emit unconditionally.
                    let effort_str = match thinking.effort {
                        ThinkingEffort::Low => "low",
                        ThinkingEffort::Medium => "medium",
                        ThinkingEffort::High => "high",
                        ThinkingEffort::XHigh => "xhigh",
                    };
                    let mut r = json!({ "effort": effort_str });
                    if let Some(b) = thinking.budget_tokens {
                        r["max_tokens"] = json!(b);
                    }
                    body["reasoning"] = r;
                }
            }
        }

        // Prompt caching: keep repeated turns landing on the same cached prefix.
        // The stable-routing field differs by provider:
        // - OpenAI (`OpenAiEffort`) has a real top-level `prompt_cache_key`.
        // - OpenRouter has NO `prompt_cache_key`; it uses top-level `session_id`
        //   (≤256 chars) for sticky routing so a session's turns reuse the same
        //   upstream provider instance — the affinity that makes its (implicit or
        //   `cache_control`-marked) cache actually hit. See OpenRouter prompt-
        //   caching docs.
        // - OpenCode Go supports both `prompt_cache_key` (clamped to 64 chars) and
        //   `prompt_cache_retention: "24h"` to keep the session prefix alive
        //   across turns and longer pauses.
        // Other OpenAI-compatible providers ignore unknown fields.
        if let Some(ref key) = req.prompt_cache_key {
            match self.provider.capabilities.reasoning {
                crate::llm::provider::ReasoningStyle::OpenAiEffort => {
                    body["prompt_cache_key"] = json!(key);
                }
                crate::llm::provider::ReasoningStyle::OpenRouterUnified => {
                    // session_id is capped at 256 chars by OpenRouter.
                    let sid: String = key.chars().take(256).collect();
                    body["session_id"] = json!(sid);
                }
                crate::llm::provider::ReasoningStyle::OpenCodeGo => {
                    // GLM/Zhipu upstreams reject cache_control markers, so skip
                    // all cache instrumentation for those models and let the
                    // request go out unchanged. Other OpenCode Go models get the
                    // full recipe.
                    if opencode_cache_supported {
                        let k: String = key.chars().take(64).collect();
                        body["prompt_cache_key"] = json!(k);
                        body["prompt_cache_retention"] = json!("24h");
                    }
                }
                crate::llm::provider::ReasoningStyle::None => {}
            }
        }

        body
    }
}

#[async_trait::async_trait]
impl LlmDriver for OpenAiDriver {
    /// #1045: the resolved per-request timeout, doubling as the
    /// idle-gap budget on the streaming turn path (#1044).
    fn request_timeout(&self) -> std::time::Duration {
        self.provider.request_timeout
    }

    /// The separately-configured first-byte budget, when any (#1044).
    fn ttfb_timeout(&self) -> Option<std::time::Duration> {
        self.provider.ttfb_timeout
    }

    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let body = self.build_body(req, false);

        // Debug log the request body (trace level for full body)
        tracing::debug!(target: "llm::openai", model=%self.provider.model, "Sending LLM request");
        if tracing::enabled!(tracing::Level::TRACE) {
            if let Ok(pretty) = serde_json::to_string_pretty(&body) {
                tracing::trace!(target: "llm::openai", "Full request body:\n{}", pretty);
            }
        }

        let complete_timeout = self.provider.request_timeout;
        // Bound total wall-clock so a slow endpoint can't multiply the
        // per-request timeout across retries.
        let retry_deadline = crate::llm::retry_deadline(complete_timeout);
        let loop_start = std::time::Instant::now();
        for attempt in 0..=crate::llm::MAX_CONNECTION_RETRIES {
            let builder = self.apply_auth(
                self.client
                    .post(&self.provider.base_url)
                    .timeout(complete_timeout)
                    .header("Content-Type", "application/json")
                    .json(&body),
            );

            let response = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    if let Some(wait_ms) = crate::llm::next_connection_retry_wait(
                        &e,
                        attempt,
                        loop_start.elapsed(),
                        retry_deadline,
                    ) {
                        crate::llm::log_transport_retry(
                            crate::llm::classify_transport_error(&e),
                            attempt,
                            wait_ms,
                            loop_start.elapsed(),
                            &e,
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                    return Err(crate::llm::transport_terminal_error(
                        crate::llm::classify_transport_error(&e),
                        attempt + 1,
                        loop_start.elapsed(),
                        &e,
                    ));
                }
            };
            let status = response.status();

            if status.as_u16() == 429 || status.as_u16() == 529 {
                if attempt < crate::llm::MAX_CONNECTION_RETRIES {
                    let wait_ms = (attempt + 1) as u64 * 2000;
                    tracing::warn!(
                        status = status.as_u16(),
                        attempt,
                        wait_ms,
                        "Rate limited, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                anyhow::bail!("OpenAI API rate limited after {} retries", crate::llm::MAX_CONNECTION_RETRIES);
            }

            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                if crate::llm::is_context_overflow_error(status.as_u16(), &text) {
                    anyhow::bail!(
                        "context_overflow: provider=openai status={} detail={}", status, text
                    );
                }
                if let Some(wait_ms) = crate::llm::next_server_error_retry_wait(
                    status.as_u16(),
                    &text,
                    attempt,
                    loop_start.elapsed(),
                    retry_deadline,
                ) {
                    tracing::warn!(
                        status = status.as_u16(),
                        attempt,
                        wait_ms,
                        "LLM transient server error, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                tracing::warn!(
                    target: "llm::openai",
                    status = %status,
                    model = %self.provider.model,
                    "LLM API error response"
                );
                anyhow::bail!("OpenAI API error {}: {}", status, text);
            }

            let body_text = match response.text().await {
                Ok(t) => t,
                Err(e) => {
                    if let Some(wait_ms) = crate::llm::next_body_read_retry_wait(
                        crate::llm::error_is_timeout(&e),
                        attempt,
                        loop_start.elapsed(),
                        retry_deadline,
                    ) {
                        tracing::warn!(
                            attempt,
                            wait_ms,
                            elapsed_ms = loop_start.elapsed().as_millis() as u64,
                            transport_kind = "body",
                            error = %e,
                            error_source_chain = %crate::llm::transport_error_source_chain(&e),
                            "LLM response body read failed, retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                    return Err(crate::llm::transport_terminal_error(
                        crate::llm::classify_transport_error(&e),
                        attempt + 1,
                        loop_start.elapsed(),
                        &e,
                    ));
                }
            };
            let j: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
                tracing::warn!(
                    target: "llm::openai",
                    model = %self.provider.model,
                    body_len = body_text.len(),
                    body_preview = %String::from_utf8_lossy(&body_text.as_bytes()[..body_text.len().min(512)]),
                    "LLM response body is not valid JSON"
                );
                anyhow::anyhow!(
                    "error decoding LLM response body as JSON: {} (body_len={}, preview={:?})",
                    e,
                    body_text.len(),
                    &body_text[..body_text.len().min(256)]
                )
            })?;
            return Ok(parse_response(&j));
        }
        tracing::warn!(
            target: "llm::openai",
            model = %self.provider.model,
            "OpenAI complete() fell through retry loop"
        );
        anyhow::bail!(
            "OpenAI complete() retries exhausted for model {}",
            self.provider.model
        );
    }

    async fn stream(
        &self,
        req: &CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<CompletionResponse> {
        use futures::StreamExt;

        if !self.provider.capabilities.supports_streaming {
            // Fall back to complete() and emit one chunk
            return super::LlmDriver::stream(self as &dyn super::LlmDriver, req, tx).await;
        }

        let body = self.build_body(req, true);

        let complete_timeout = self.provider.request_timeout;
        let retry_deadline = crate::llm::retry_deadline(complete_timeout);
        let loop_start = std::time::Instant::now();
        'retry: for attempt in 0..=crate::llm::MAX_CONNECTION_RETRIES {
            let builder = self.apply_auth(
                self.client
                    .post(&self.provider.base_url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .json(&body),
            );

            let response = match builder.send().await {
                Ok(r) => r,
                Err(e) if crate::llm::is_transient_connection_error(&e) && attempt < crate::llm::MAX_CONNECTION_RETRIES => {
                    let wait_ms = crate::llm::connection_retry_backoff_ms(attempt);
                    crate::llm::log_transport_retry(
                        crate::llm::classify_transport_error(&e),
                        attempt,
                        wait_ms,
                        loop_start.elapsed(),
                        &e,
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                Err(e) if crate::llm::is_transient_connection_error(&e) => {
                    return Err(crate::llm::transport_terminal_error(
                        crate::llm::classify_transport_error(&e),
                        attempt + 1,
                        loop_start.elapsed(),
                        &e,
                    ));
                }
                Err(e) => return Err(e.into()),
            };
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                if crate::llm::is_context_overflow_error(status.as_u16(), &text) {
                    anyhow::bail!(
                        "context_overflow: provider=openai status={} detail={}", status, text
                    );
                }
                if let Some(wait_ms) = crate::llm::next_server_error_retry_wait(
                    status.as_u16(),
                    &text,
                    attempt,
                    loop_start.elapsed(),
                    retry_deadline,
                ) {
                    tracing::warn!(
                        status = status.as_u16(),
                        attempt,
                        wait_ms,
                        "LLM transient server error in stream, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                tracing::warn!(
                    target: "autonoetic::llm::openai",
                    status = %status,
                    response_text = %text,
                    "OpenAI stream error"
                );
                anyhow::bail!("OpenAI stream error {}: {}", status, text);
            }

            // Some OpenAI-compatible endpoints (and proxies) answer a
            // `stream: true` request with a plain JSON completion instead of
            // SSE. Degrade gracefully: parse it as a normal response and emit
            // it as a single chunk, rather than hanging or failing on a body
            // that is not event-stream framed.
            let is_event_stream = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.starts_with("text/event-stream"))
                .unwrap_or(false);
            if !is_event_stream {
                let body_text = response.text().await.map_err(|e| {
                    crate::llm::transport_terminal_error(
                        crate::llm::classify_transport_error(&e),
                        attempt + 1,
                        loop_start.elapsed(),
                        &e,
                    )
                })?;
                let j: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
                    anyhow::anyhow!(
                        "error decoding LLM response body as JSON: {} (body_len={}, preview={:?})",
                        e,
                        body_text.len(),
                        &body_text[..body_text.len().min(256)]
                    )
                })?;
                let response = parse_response(&j);
                if !response.text.is_empty() {
                    let _ = tx.send(StreamEvent::TextDelta(response.text.clone())).await;
                }
                let _ = tx
                    .send(StreamEvent::Complete {
                        stop_reason: response.stop_reason.clone(),
                        usage: response.usage.clone(),
                    })
                    .await;
                return Ok(response);
            }

            let mut text_accum = String::new();
            let mut reasoning_accum = String::new();
            let mut reasoning_details_accum: Vec<serde_json::Value> = Vec::new();
            let mut tool_calls_accum: Vec<ToolCall> = Vec::new();
            let mut stop_reason = StopReason::EndTurn;
            let mut usage = TokenUsage::default();
            let mut buffer = String::new();
            let mut byte_stream = response.bytes_stream();

            while let Some(chunk) = byte_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        // Retry only if no TextDelta has been emitted on `tx`
                        // yet — otherwise a retry would replay duplicated
                        // deltas to the consumer.
                        if text_accum.is_empty() {
                            if let Some(wait_ms) = crate::llm::next_body_read_retry_wait(
                                crate::llm::error_is_timeout(&e),
                                attempt,
                                loop_start.elapsed(),
                                retry_deadline,
                            ) {
                                tracing::warn!(
                                    attempt,
                                    wait_ms,
                                    elapsed_ms = loop_start.elapsed().as_millis() as u64,
                                    transport_kind = "body",
                                    error = %e,
                                    error_source_chain = %crate::llm::transport_error_source_chain(&e),
                                    "LLM stream body read failed before any delta, retrying"
                                );
                                tokio::time::sleep(std::time::Duration::from_millis(wait_ms))
                                    .await;
                                continue 'retry;
                            }
                        }
                        return Err(crate::llm::transport_terminal_error(
                            crate::llm::classify_transport_error(&e),
                            attempt + 1,
                            loop_start.elapsed(),
                            &e,
                        ));
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find("\n\n") {
                    let event_text = buffer[..pos].to_string();
                    buffer = buffer[pos + 2..].to_string();

                    let data = event_text
                        .lines()
                        .find_map(|l| l.strip_prefix("data: "))
                        .unwrap_or("")
                        .trim()
                        .to_string();

                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }

                    let Ok(j) = serde_json::from_str::<serde_json::Value>(&data) else {
                        tracing::debug!(
                            target: "llm::openai",
                            data_preview = %&data[..data.len().min(128)],
                            "Malformed SSE JSON chunk skipped"
                        );
                        continue;
                    };
                    let delta = &j["choices"][0]["delta"];

                    if let Some(text) = delta["content"].as_str() {
                        if !text.is_empty() {
                            text_accum.push_str(text);
                            let _ = tx.send(StreamEvent::TextDelta(text.to_string())).await;
                        }
                    }

                    // Reasoning text streams under `reasoning_content` (DeepSeek-
                    // direct / OpenAI-compatible) or `reasoning` (OpenRouter).
                    if let Some(reasoning) = delta["reasoning_content"].as_str() {
                        if !reasoning.is_empty() {
                            reasoning_accum.push_str(reasoning);
                        }
                    }
                    if let Some(reasoning) = delta["reasoning"].as_str() {
                        if !reasoning.is_empty() {
                            reasoning_accum.push_str(reasoning);
                        }
                    }
                    // OpenRouter streams structured reasoning blocks incrementally.
                    if let Some(details) = delta["reasoning_details"].as_array() {
                        reasoning_details_accum.extend(details.iter().cloned());
                    }

                    if self.provider.capabilities.supports_tool_stream_deltas {
                        if let Some(tcs) = delta["tool_calls"].as_array() {
                            for tc_delta in tcs {
                                let idx = tc_delta["index"].as_u64().unwrap_or(0) as usize;
                                while tool_calls_accum.len() <= idx {
                                    tool_calls_accum.push(ToolCall {
                                        id: String::new(),
                                        name: String::new(),
                                        arguments: String::new(),
                                    });
                                }
                                if let Some(id) = tc_delta["id"].as_str() {
                                    tool_calls_accum[idx].id = id.to_string();
                                }
                                if let Some(name) = tc_delta["function"]["name"].as_str() {
                                    tool_calls_accum[idx].name = name.to_string();
                                }
                                if let Some(args) = tc_delta["function"]["arguments"].as_str() {
                                    tool_calls_accum[idx].arguments.push_str(args);
                                }
                            }
                        }
                    }

                    if let Some(reason) = j["choices"][0]["finish_reason"].as_str() {
                        stop_reason = parse_stop_reason(reason);
                    }

                    // Usage typically arrives on the final chunk (requires
                    // `stream_options.include_usage`; harmless when absent).
                    if j["usage"].is_object() {
                        usage = parse_usage(&j["usage"]);
                    }
                }
            }

            // Fallback: extract XML-style tool calls from accumulated text when
            // the structured `tool_calls` deltas never arrived (mirrors the
            // non-streaming `parse_response` fallback — models with XML-based
            // chat templates may emit `<tool_call>` blocks as plain text).
            if tool_calls_accum.is_empty() && text_accum.contains("<tool_call>") {
                let (_reasoning, xml_calls) =
                    crate::llm::xml_tool_calls::extract_xml_tool_calls(&text_accum);
                if !xml_calls.is_empty() {
                    tracing::info!(
                        target: "llm::openai",
                        count = xml_calls.len(),
                        "Extracted XML tool calls from streamed text fallback"
                    );
                    tool_calls_accum = xml_calls;
                }
            }

            for tc in &tool_calls_accum {
                let _ = tx
                    .send(StreamEvent::ToolUseEnd {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .await;
            }

            let resp = CompletionResponse {
                text: text_accum,
                tool_calls: tool_calls_accum,
                reasoning_content: if reasoning_accum.is_empty() {
                    None
                } else {
                    Some(reasoning_accum)
                },
                reasoning_details: if reasoning_details_accum.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Array(reasoning_details_accum))
                },
                stop_reason: stop_reason.clone(),
                usage,
            };
            let _ = tx
                .send(StreamEvent::Complete {
                    stop_reason,
                    usage: resp.usage.clone(),
                })
                .await;
            return Ok(resp);
        }
        tracing::warn!(
            target: "llm::openai",
            model = %self.provider.model,
            "OpenAI stream() max connection retries exceeded"
        );
        anyhow::bail!(
            "OpenAI stream() max connection retries exceeded for model {}",
            self.provider.model
        );
    }
}

/// Parse a non-streaming JSON response body.
fn parse_response(j: &serde_json::Value) -> CompletionResponse {
    let choices = j["choices"].as_array();
    if choices.is_none_or(|a| a.is_empty()) {
        tracing::warn!(
            target: "llm::openai",
            has_choices = choices.is_some(),
            "OpenAI response has no choices array — returning empty completion"
        );
    }
    let choice = &j["choices"][0];
    let text = extract_text_content(&choice["message"]["content"]);
    let reasoning_content = extract_reasoning_content(&choice["message"]);

    let mut tool_calls: Vec<ToolCall> = choice["message"]["tool_calls"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|tc| {
                    let id = tc["id"].as_str()?.to_string();
                    let name = tc["function"]["name"].as_str()?.to_string();
                    let args_val = &tc["function"]["arguments"];
                    let arguments = if args_val.is_string() {
                        args_val.as_str().unwrap_or("{}").to_string()
                    } else if args_val.is_object() {
                        serde_json::to_string(args_val).unwrap_or_else(|_| "{}".to_string())
                    } else {
                        "{}".to_string()
                    };
                    Some(ToolCall {
                        id,
                        name,
                        arguments,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Fallback: extract XML-style tool calls from the text content when the
    // structured `tool_calls` field is empty but the response contains
    // `<tool_call>` blocks. This handles models that use XML-based chat
    // templates (e.g. Qwen 3.5 with qwen35-template.jinja) where the server
    // may not extract tool calls into the structured JSON field.
    if tool_calls.is_empty() && text.contains("<tool_call>") {
        let (_reasoning, xml_calls) =
            crate::llm::xml_tool_calls::extract_xml_tool_calls(&text);
        if !xml_calls.is_empty() {
            tracing::info!(
                target: "llm::openai",
                count = xml_calls.len(),
                "Extracted XML tool calls from response text fallback"
            );
            tool_calls = xml_calls;
        }
    }

    let stop_reason = parse_stop_reason(choice["finish_reason"].as_str().unwrap_or(""));

    let reasoning_details = extract_reasoning_details(&choice["message"]);

    let usage = parse_usage(&j["usage"]);
    if j.get("usage").is_none() {
        tracing::warn!(
            target: "llm::openai",
            "OpenAI response missing usage block — token counts will be zero"
        );
    }

    CompletionResponse {
        text,
        tool_calls,
        reasoning_content,
        reasoning_details,
        stop_reason,
        usage,
    }
}

/// Parse a `usage` object, including reasoning/cache token details when the
/// provider reports them (OpenAI/OpenRouter `*_tokens_details`).
fn parse_usage(usage: &serde_json::Value) -> TokenUsage {
    TokenUsage {
        input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
        reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0),
        cached_tokens: usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
    }
}

/// Capture the model's reasoning text from whichever field the provider uses.
/// OpenRouter returns `reasoning` (and structured `reasoning_details`);
/// DeepSeek-direct and OpenAI-compatible reasoning models return
/// `reasoning_content`. Falls back to flattening `reasoning_details[].text`.
fn extract_reasoning_content(message: &serde_json::Value) -> Option<String> {
    if let Some(s) = message["reasoning_content"].as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(s) = message["reasoning"].as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    // Flatten reasoning_details[].text|.summary into a single string.
    if let Some(arr) = message["reasoning_details"].as_array() {
        let mut out = String::new();
        for block in arr {
            if let Some(t) = block["text"].as_str().filter(|s| !s.is_empty()) {
                out.push_str(t);
            } else if let Some(t) = block["summary"].as_str().filter(|s| !s.is_empty()) {
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// Capture the raw `reasoning_details` array verbatim so it can be replayed on
/// the next assistant turn (required for signed/encrypted reasoning blocks).
fn extract_reasoning_details(message: &serde_json::Value) -> Option<serde_json::Value> {
    match &message["reasoning_details"] {
        serde_json::Value::Array(arr) if !arr.is_empty() => {
            Some(serde_json::Value::Array(arr.clone()))
        }
        _ => None,
    }
}

fn extract_text_content(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                out.push_str(s);
                continue;
            }
            if let Some(s) = item["text"].as_str() {
                out.push_str(s);
                continue;
            }
            if let Some(s) = item["content"].as_str() {
                out.push_str(s);
                continue;
            }
        }
        return out;
    }
    if let Some(s) = content["text"].as_str() {
        return s.to_string();
    }
    if let Some(s) = content["content"].as_str() {
        return s.to_string();
    }
    String::new()
}

fn parse_stop_reason(s: &str) -> StopReason {
    match s {
        "stop" | "end_turn" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "tool_use" => StopReason::ToolUse,
        other => StopReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_response_with_string_content() {
        let j = json!({
            "choices": [{
                "message": { "content": "hello" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2 }
        });
        let resp = parse_response(&j);
        assert_eq!(resp.text, "hello");
    }

    #[test]
    fn test_parse_response_with_array_content_blocks() {
        let j = json!({
            "choices": [{
                "message": {
                    "content": [
                        {"type": "text", "text": "hello "},
                        {"type": "text", "text": "world"}
                    ]
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2 }
        });
        let resp = parse_response(&j);
        assert_eq!(resp.text, "hello world");
    }

    #[test]
    fn test_parse_tool_calls_with_object_arguments() {
        let j = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "content_write",
                            "arguments": {"name": "test.py", "content": "hello"}
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        });
        let resp = parse_response(&j);
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "content_write");
        assert_eq!(
            resp.tool_calls[0].arguments,
            r#"{"content":"hello","name":"test.py"}"#
        );
    }

    // -----------------------------------------------------------------------
    // Reasoning / thinking dispatch
    // -----------------------------------------------------------------------

    use crate::llm::provider::{
        AuthStrategy, DriverKind, ProviderCapabilities, ReasoningStyle, ResolvedProvider,
    };
    use crate::llm::{CompletionRequest, Message};
    use autonoetic_types::agent::{ThinkingConfig, ThinkingEffort};

    fn driver_with(model: &str, reasoning: ReasoningStyle) -> OpenAiDriver {
        let mut caps = ProviderCapabilities::openai_compatible();
        caps.reasoning = reasoning;
        OpenAiDriver::new(
            Client::new(),
            ResolvedProvider {
                kind: DriverKind::OpenAi,
                base_url: "http://test.invalid".to_string(),
                model: model.to_string(),
                auth: AuthStrategy::None,
                capabilities: caps,
                extra_headers: vec![],
                temperature: None,
                max_tokens: None,
                egress_class: autonoetic_types::egress::EgressClass::Remote,
                request_timeout: std::time::Duration::from_secs(120),
                ttfb_timeout: None,
            },
        )
    }

    fn req_with_thinking(model: &str, effort: ThinkingEffort, budget: Option<u32>) -> CompletionRequest {
        CompletionRequest {
            model: model.to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: None,
            thinking: Some(ThinkingConfig { effort, budget_tokens: budget }),
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        }
    }

    #[test]
    fn reasoning_none_omits_field() {
        let driver = driver_with("o3-mini", ReasoningStyle::None);
        let body = driver.build_body(&req_with_thinking("o3-mini", ThinkingEffort::High, None), false);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_openai_effort_emits_top_level_string_for_reasoning_models() {
        let driver = driver_with("o3-mini", ReasoningStyle::OpenAiEffort);
        let body = driver.build_body(&req_with_thinking("o3-mini", ThinkingEffort::Medium, None), false);
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn reasoning_openai_effort_dropped_for_non_reasoning_models() {
        // OpenAI rejects reasoning_effort on non-reasoning models (e.g. gpt-4o),
        // so we gate by model name even when the provider supports it.
        let driver = driver_with("gpt-4o", ReasoningStyle::OpenAiEffort);
        let body = driver.build_body(&req_with_thinking("gpt-4o", ThinkingEffort::High, None), false);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn reasoning_openrouter_emits_object_unconditionally() {
        // OpenRouter ignores `reasoning` on non-reasoning models, so we emit
        // it whenever thinking is set — no model-name gate.
        let driver = driver_with(
            "deepseek/deepseek-v4-flash",
            ReasoningStyle::OpenRouterUnified,
        );
        let body = driver.build_body(
            &req_with_thinking(
                "deepseek/deepseek-v4-flash",
                ThinkingEffort::High,
                Some(8000),
            ),
            false,
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["max_tokens"], 8000);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_xhigh_emits_literal_on_openrouter() {
        // OpenRouter exposes a distinct "xhigh" tier (e.g. DeepSeek V4 Flash
        // maps xhigh → max reasoning), so we pass it through literally.
        let or = driver_with("deepseek/deepseek-v4-flash", ReasoningStyle::OpenRouterUnified);
        let body = or.build_body(
            &req_with_thinking("deepseek/deepseek-v4-flash", ThinkingEffort::XHigh, None),
            false,
        );
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn reasoning_xhigh_collapses_to_high_on_openai() {
        // OpenAI's reasoning_effort only accepts low|medium|high — XHigh
        // collapses to "high" to avoid an API rejection.
        let openai = driver_with("o3-mini", ReasoningStyle::OpenAiEffort);
        let body = openai.build_body(
            &req_with_thinking("o3-mini", ThinkingEffort::XHigh, None),
            false,
        );
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn thinking_effort_xhigh_serde_rename() {
        // YAML round-trip uses the literal `xhigh` token (not the default
        // snake_case `x_high`) so configs match OpenRouter's documented value.
        let yaml = serde_yaml::to_string(&ThinkingEffort::XHigh).unwrap();
        assert_eq!(yaml.trim(), "xhigh");
        let parsed: ThinkingEffort = serde_yaml::from_str("xhigh").unwrap();
        assert_eq!(parsed, ThinkingEffort::XHigh);
    }

    // -----------------------------------------------------------------------
    // Reasoning capture (gap 1)
    // -----------------------------------------------------------------------

    #[test]
    fn capture_reasoning_content_field() {
        let msg = json!({ "reasoning_content": "step by step" });
        assert_eq!(extract_reasoning_content(&msg).as_deref(), Some("step by step"));
    }

    #[test]
    fn capture_openrouter_reasoning_field() {
        // OpenRouter returns the text under `reasoning`, not `reasoning_content`.
        let msg = json!({ "reasoning": "openrouter thoughts" });
        assert_eq!(
            extract_reasoning_content(&msg).as_deref(),
            Some("openrouter thoughts")
        );
    }

    #[test]
    fn capture_reasoning_details_flattened_to_text() {
        let msg = json!({
            "reasoning_details": [
                { "type": "reasoning.text", "text": "first " },
                { "type": "reasoning.text", "text": "second" }
            ]
        });
        assert_eq!(
            extract_reasoning_content(&msg).as_deref(),
            Some("first second")
        );
    }

    #[test]
    fn capture_reasoning_details_raw_preserved() {
        let msg = json!({
            "reasoning_details": [
                { "type": "reasoning.encrypted", "data": "abc", "format": "openai" }
            ]
        });
        let details = extract_reasoning_details(&msg).expect("details captured");
        assert_eq!(details[0]["data"], "abc");
        assert_eq!(details[0]["format"], "openai");
    }

    #[test]
    fn capture_reasoning_details_empty_is_none() {
        assert!(extract_reasoning_details(&json!({ "reasoning_details": [] })).is_none());
        assert!(extract_reasoning_details(&json!({})).is_none());
    }

    // -----------------------------------------------------------------------
    // Usage details (gap 4)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_usage_captures_reasoning_and_cache_tokens() {
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 200,
            "completion_tokens_details": { "reasoning_tokens": 150 },
            "prompt_tokens_details": { "cached_tokens": 800 }
        });
        let u = parse_usage(&usage);
        assert_eq!(u.input_tokens, 1000);
        assert_eq!(u.output_tokens, 200);
        assert_eq!(u.reasoning_tokens, 150);
        assert_eq!(u.cached_tokens, 800);
    }

    #[test]
    fn parse_usage_defaults_to_zero_when_absent() {
        let u = parse_usage(&json!({ "prompt_tokens": 5, "completion_tokens": 7 }));
        assert_eq!(u.reasoning_tokens, 0);
        assert_eq!(u.cached_tokens, 0);
    }

    // -----------------------------------------------------------------------
    // Round-trip (gap 2) + cache key (gap 3) in the request body
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_prefers_reasoning_details_over_content() {
        let driver = driver_with("deepseek/deepseek-v4-flash", ReasoningStyle::OpenRouterUnified);
        let mut assistant = Message::assistant("answer");
        assistant.reasoning_content = Some("plain".to_string());
        assistant.reasoning_details = Some(json!([{ "type": "reasoning.text", "text": "structured" }]));
        let req = CompletionRequest {
            model: "deepseek/deepseek-v4-flash".to_string(),
            messages: vec![assistant],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: None,
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        };
        let body = driver.build_body(&req, false);
        // reasoning_details replayed; plain reasoning_content suppressed.
        assert!(body["messages"][0]["reasoning_details"].is_array());
        assert!(body["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn round_trip_falls_back_to_reasoning_content() {
        let driver = driver_with("deepseek-reasoner", ReasoningStyle::None);
        let mut assistant = Message::assistant("answer");
        assistant.reasoning_content = Some("plain".to_string());
        let req = CompletionRequest {
            model: "deepseek-reasoner".to_string(),
            messages: vec![assistant],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: None,
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        };
        let body = driver.build_body(&req, false);
        assert_eq!(body["messages"][0]["reasoning_content"], "plain");
    }

    #[test]
    fn openrouter_uses_session_id_not_prompt_cache_key() {
        // OpenRouter has no prompt_cache_key; the stable-routing field is
        // top-level `session_id` (sticky routing → cache affinity).
        let driver = driver_with("deepseek/deepseek-v4-flash", ReasoningStyle::OpenRouterUnified);
        let mut req = req_with_thinking("deepseek/deepseek-v4-flash", ThinkingEffort::Low, None);
        req.prompt_cache_key = Some("session-abc".to_string());
        let body = driver.build_body(&req, false);
        assert_eq!(body["session_id"], "session-abc");
        assert!(body.get("prompt_cache_key").is_none(), "OpenRouter must not get prompt_cache_key");
    }

    #[test]
    fn openrouter_session_id_capped_at_256_chars() {
        let driver = driver_with("anthropic/claude-sonnet-5", ReasoningStyle::OpenRouterUnified);
        let mut req = req_with_thinking("anthropic/claude-sonnet-5", ThinkingEffort::Low, None);
        req.prompt_cache_key = Some("x".repeat(300));
        let body = driver.build_body(&req, false);
        assert_eq!(body["session_id"].as_str().unwrap().chars().count(), 256);
    }

    #[test]
    fn openai_emits_prompt_cache_key() {
        // Direct OpenAI (OpenAiEffort) uses the real top-level prompt_cache_key.
        let driver = driver_with("gpt-5", ReasoningStyle::OpenAiEffort);
        let mut req = req_with_thinking("gpt-5", ThinkingEffort::Low, None);
        req.prompt_cache_key = Some("session-abc".to_string());
        let body = driver.build_body(&req, false);
        assert_eq!(body["prompt_cache_key"], "session-abc");
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn plain_provider_gets_neither_cache_routing_field() {
        // ReasoningStyle::None (e.g. groq, llama.cpp) gets neither field.
        let driver = driver_with("llama-3.1-70b", ReasoningStyle::None);
        let mut req = req_with_thinking("llama-3.1-70b", ThinkingEffort::Low, None);
        req.prompt_cache_key = Some("session-abc".to_string());
        let body = driver.build_body(&req, false);
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("session_id").is_none());
    }

    fn req_with_system(model: &str, system: &str, prefix_bytes: Option<usize>) -> CompletionRequest {
        CompletionRequest {
            model: model.to_string(),
            messages: vec![Message::system(system), Message::user("hi")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: None,
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: prefix_bytes,
        }
    }

    #[test]
    fn openrouter_claude_marks_system_cache_prefix() {
        let driver = driver_with("anthropic/claude-sonnet-5", ReasoningStyle::OpenRouterUnified);
        let system = "STABLE DOCTRINE\n\n[state] volatile";
        let prefix = "STABLE DOCTRINE".len();
        let body = driver.build_body(&req_with_system("anthropic/claude-sonnet-5", system, Some(prefix)), false);
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "system content must be structured, got {content}");
        assert_eq!(content[0]["text"], "STABLE DOCTRINE");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert!(content[1]["cache_control"].is_null(), "volatile suffix must not be cached");
        assert!(content[1]["text"].as_str().unwrap().contains("volatile"));
    }

    #[test]
    fn openrouter_openai_model_leaves_system_plain() {
        // OpenAI-family model via OpenRouter caches automatically — no cache_control.
        let driver = driver_with("openai/gpt-5", ReasoningStyle::OpenRouterUnified);
        let system = "STABLE DOCTRINE\n\n[state] volatile";
        let prefix = "STABLE DOCTRINE".len();
        let body = driver.build_body(&req_with_system("openai/gpt-5", system, Some(prefix)), false);
        assert!(body["messages"][0]["content"].is_string(), "non-Claude/Gemini stays a plain string");
    }

    #[test]
    fn plain_provider_never_gets_cache_control() {
        // llama.cpp / groq (ReasoningStyle::None) rely on automatic prefix reuse.
        let driver = driver_with("some-local-model", ReasoningStyle::None);
        let system = "STABLE DOCTRINE\n\n[state] volatile";
        let prefix = "STABLE DOCTRINE".len();
        let body = driver.build_body(&req_with_system("some-local-model", system, Some(prefix)), false);
        assert!(body["messages"][0]["content"].is_string());
    }

    #[test]
    fn openrouter_cache_control_gating_by_model() {
        assert!(model_supports_openrouter_cache_control("anthropic/claude-sonnet-5"));
        assert!(model_supports_openrouter_cache_control("google/gemini-2.5-pro"));
        assert!(model_supports_openrouter_cache_control("some/claude-clone"));
        assert!(!model_supports_openrouter_cache_control("openai/gpt-5"));
        assert!(!model_supports_openrouter_cache_control("deepseek/deepseek-v4"));
    }

    // -----------------------------------------------------------------------
    // Tools-array caching (cache_control on the last tool definition)
    // -----------------------------------------------------------------------

    use crate::llm::ToolDefinition;

    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("desc for {name}"),
            input_schema: json!({"type": "object"}),
        }
    }

    fn req_with_tools(
        model: &str,
        tools: Vec<ToolDefinition>,
        prefix_bytes: Option<usize>,
    ) -> CompletionRequest {
        let mut req = req_with_system(model, "system", prefix_bytes);
        req.tools = tools;
        req
    }

    #[test]
    fn openrouter_claude_marks_last_tool_with_cache_control() {
        let driver = driver_with("anthropic/claude-sonnet-5", ReasoningStyle::OpenRouterUnified);
        let tools = vec![tool_def("alpha"), tool_def("beta"), tool_def("gamma")];
        let req = req_with_tools(
            "anthropic/claude-sonnet-5",
            tools,
            Some("system".len()),
        );
        let body = driver.build_body(&req, false);
        let tools_arr = body["tools"].as_array().expect("tools array");
        assert_eq!(tools_arr.len(), 3);
        assert!(tools_arr[0]["function"]["cache_control"].is_null(), "first tool must not be marked");
        assert!(tools_arr[1]["function"]["cache_control"].is_null(), "middle tool must not be marked");
        assert_eq!(tools_arr[2]["function"]["cache_control"]["type"], "ephemeral");
        // Schemas preserved.
        assert_eq!(tools_arr[2]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn openrouter_gemini_marks_last_tool_with_cache_control() {
        let driver = driver_with("google/gemini-2.5-pro", ReasoningStyle::OpenRouterUnified);
        let tools = vec![tool_def("solo")];
        let req = req_with_tools(
            "google/gemini-2.5-pro",
            tools,
            Some("system".len()),
        );
        let body = driver.build_body(&req, false);
        assert_eq!(body["tools"][0]["function"]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn openrouter_claude_leaves_tools_plain_without_prefix() {
        // No system_cache_prefix_bytes → caching off → tools unmarked.
        let driver = driver_with("anthropic/claude-sonnet-5", ReasoningStyle::OpenRouterUnified);
        let tools = vec![tool_def("alpha"), tool_def("beta")];
        let req = req_with_tools(
            "anthropic/claude-sonnet-5",
            tools,
            None,
        );
        let body = driver.build_body(&req, false);
        let tools_arr = body["tools"].as_array().unwrap();
        assert!(tools_arr[0]["function"]["cache_control"].is_null());
        assert!(tools_arr[1]["function"]["cache_control"].is_null());
    }

    #[test]
    fn openrouter_openai_model_leaves_tools_plain() {
        // OpenAI-family via OpenRouter caches automatically — no cache_control on tools.
        let driver = driver_with("openai/gpt-5", ReasoningStyle::OpenRouterUnified);
        let tools = vec![tool_def("alpha"), tool_def("beta")];
        let req = req_with_tools(
            "openai/gpt-5",
            tools,
            Some("system".len()),
        );
        let body = driver.build_body(&req, false);
        let tools_arr = body["tools"].as_array().unwrap();
        assert!(tools_arr[0]["function"]["cache_control"].is_null());
        assert!(tools_arr[1]["function"]["cache_control"].is_null());
    }

    #[test]
    fn direct_openai_leaves_tools_plain() {
        // Direct OpenAI (OpenAiEffort) uses prompt_cache_key, never cache_control.
        let driver = driver_with("gpt-5", ReasoningStyle::OpenAiEffort);
        let tools = vec![tool_def("alpha"), tool_def("beta")];
        let req = req_with_tools(
            "gpt-5",
            tools,
            Some("system".len()),
        );
        let body = driver.build_body(&req, false);
        let tools_arr = body["tools"].as_array().unwrap();
        assert!(tools_arr[0]["function"]["cache_control"].is_null());
        assert!(tools_arr[1]["function"]["cache_control"].is_null());
    }



    #[test]
    fn stream_request_includes_usage_option() {
        let driver = driver_with("gpt-4o", ReasoningStyle::None);
        let req = CompletionRequest::simple("gpt-4o", vec![Message::user("hi")]);
        let body = driver.build_body(&req, true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    // -----------------------------------------------------------------------
    // OpenCode Go prompt caching
    // -----------------------------------------------------------------------

    fn opencode_tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("desc for {name}"),
            input_schema: json!({"type": "object"}),
        }
    }

    fn opencode_req_with_tools(
        model: &str,
        tools: Vec<ToolDefinition>,
        prefix_bytes: Option<usize>,
    ) -> CompletionRequest {
        let mut req = req_with_system(model, "system", prefix_bytes);
        req.tools = tools;
        req
    }

    #[test]
    fn opencode_emits_prompt_cache_key_and_retention() {
        let driver = driver_with("deepseek-v4-flash", ReasoningStyle::OpenCodeGo);
        let mut req = req_with_system("deepseek-v4-flash", "system", None);
        req.prompt_cache_key = Some("session-abc".to_string());
        let body = driver.build_body(&req, false);
        assert_eq!(body["prompt_cache_key"], "session-abc");
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert!(body.get("session_id").is_none());
    }

    #[test]
    fn opencode_prompt_cache_key_clamped_to_64_chars() {
        let driver = driver_with("deepseek-v4-flash", ReasoningStyle::OpenCodeGo);
        let mut req = req_with_system("deepseek-v4-flash", "system", None);
        req.prompt_cache_key = Some("x".repeat(100));
        let body = driver.build_body(&req, false);
        assert_eq!(body["prompt_cache_key"].as_str().unwrap().chars().count(), 64);
        assert_eq!(body["prompt_cache_retention"], "24h");
    }

    #[test]
    fn opencode_caches_system_prefix_with_ttl() {
        let driver = driver_with("deepseek-v4-flash", ReasoningStyle::OpenCodeGo);
        let system = "STABLE DOCTRINE\n\n[state] volatile";
        let prefix = "STABLE DOCTRINE".len();
        let body = driver.build_body(&req_with_system("deepseek-v4-flash", system, Some(prefix)), false);
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "system content must be structured, got {content}");
        assert_eq!(content[0]["text"], "STABLE DOCTRINE");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(content[0]["cache_control"]["ttl"], "1h");
        assert!(content[1]["cache_control"].is_null(), "volatile suffix must not be cached");
        assert!(content[1]["text"].as_str().unwrap().contains("volatile"));
    }

    #[test]
    fn opencode_caches_last_two_user_assistant_messages() {
        let driver = driver_with("kimi-k2.7-code", ReasoningStyle::OpenCodeGo);
        // req_with_system already includes a user "hi" turn; append more turns
        // so we can verify the last two user/assistant messages are cached.
        let mut req = req_with_system("kimi-k2.7-code", "system", None);
        req.messages.push(Message::user("first user"));
        req.messages.push(Message::assistant("first assistant"));
        req.messages.push(Message::user("second user"));
        req.messages.push(Message::assistant("second assistant"));
        let body = driver.build_body(&req, false);
        // System message is cached as a whole (no prefix boundary supplied).
        assert!(body["messages"][0]["content"].is_array());
        assert_eq!(body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
        // The leading user "hi" and the next two turns stay plain strings.
        assert!(body["messages"][1]["content"].is_string());
        assert!(body["messages"][2]["content"].is_string());
        assert!(body["messages"][3]["content"].is_string());
        // Last two user/assistant turns get cache_control breakpoints.
        assert!(body["messages"][4]["content"].is_array());
        assert_eq!(body["messages"][4]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["messages"][5]["content"].is_array());
        assert_eq!(body["messages"][5]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn opencode_caches_last_tool_definition() {
        let driver = driver_with("mimo-v2.5-pro", ReasoningStyle::OpenCodeGo);
        let tools = vec![opencode_tool_def("alpha"), opencode_tool_def("beta"), opencode_tool_def("gamma")];
        let req = opencode_req_with_tools("mimo-v2.5-pro", tools, Some("system".len()));
        let body = driver.build_body(&req, false);
        let tools_arr = body["tools"].as_array().expect("tools array");
        assert_eq!(tools_arr.len(), 3);
        assert!(tools_arr[0]["function"]["cache_control"].is_null(), "first tool must not be marked");
        assert!(tools_arr[1]["function"]["cache_control"].is_null(), "middle tool must not be marked");
        assert_eq!(tools_arr[2]["function"]["cache_control"]["type"], "ephemeral");
        assert_eq!(tools_arr[2]["function"]["cache_control"]["ttl"], "1h");
        // Schemas preserved.
        assert_eq!(tools_arr[2]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn opencode_glm_skips_all_cache_stamping() {
        let driver = driver_with("glm-5.1", ReasoningStyle::OpenCodeGo);
        let mut req = req_with_system("glm-5.1", "system", Some("system".len()));
        req.prompt_cache_key = Some("session-abc".to_string());
        req.messages.push(Message::user("hi"));
        req.tools = vec![opencode_tool_def("only")];
        let body = driver.build_body(&req, false);
        // No top-level cache fields.
        assert!(body.get("prompt_cache_key").is_none(), "GLM must not get prompt_cache_key");
        assert!(body.get("prompt_cache_retention").is_none(), "GLM must not get prompt_cache_retention");
        // Content remains plain strings; no cache_control markers.
        assert!(body["messages"][0]["content"].is_string());
        assert!(body["messages"][1]["content"].is_string());
        assert!(body["tools"][0]["function"]["cache_control"].is_null());
    }

    #[test]
    fn opencode_zhipu_skips_all_cache_stamping() {
        let driver = driver_with("zhipu-glm-5", ReasoningStyle::OpenCodeGo);
        let mut req = req_with_system("zhipu-glm-5", "system", Some("system".len()));
        req.prompt_cache_key = Some("session-abc".to_string());
        let body = driver.build_body(&req, false);
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
        assert!(body["messages"][0]["content"].is_string());
    }

    #[test]
    fn sanitize_moves_type_into_anyof_branches() {
        let schema = json!({
            "type": "object",
            "properties": {
                "target_session_id": { "type": "string" },
                "target_agent_id": { "type": "string" }
            },
            "required": ["message"],
            "anyOf": [
                { "required": ["target_session_id"] },
                { "required": ["target_agent_id"] }
            ]
        });
        let sanitized = sanitize_schema_for_strict_anyof(&schema);
        assert!(sanitized.get("type").is_none(), "parent type must be removed");
        let branches = sanitized["anyOf"].as_array().unwrap();
        assert_eq!(branches[0]["type"], "object");
        assert_eq!(branches[1]["type"], "object");
    }

    #[test]
    fn sanitize_preserves_existing_branch_type() {
        let schema = json!({
            "type": "object",
            "oneOf": [
                { "type": "string" },
                { "type": "null" }
            ]
        });
        let sanitized = sanitize_schema_for_strict_anyof(&schema);
        let branches = sanitized["oneOf"].as_array().unwrap();
        assert_eq!(branches[0]["type"], "string");
        assert_eq!(branches[1]["type"], "null");
        assert!(sanitized.get("type").is_none(), "parent type must be removed");
    }

    #[test]
    fn sanitize_recurses_into_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "string",
                    "anyOf": [
                        { "enum": ["a"] },
                        { "enum": ["b"] }
                    ]
                }
            }
        });
        let sanitized = sanitize_schema_for_strict_anyof(&schema);
        let nested = &sanitized["properties"]["nested"];
        assert!(nested.get("type").is_none(), "parent type must be removed");
        assert_eq!(nested["anyOf"][0]["type"], "string");
        assert_eq!(nested["anyOf"][1]["type"], "string");
    }
}
