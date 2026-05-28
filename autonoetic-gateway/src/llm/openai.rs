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
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| {
                let mut msg = json!({ "role": m.role.as_str() });

                if !m.content.is_empty() {
                    msg["content"] = json!(m.content);
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
            body["tools"] = json!(req
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                }))
                .collect::<Vec<_>>());
            if self.provider.capabilities.supports_tool_choice {
                body["tool_choice"] = json!("auto");
            }
        }

        if let Some(ref thinking) = req.thinking {
            use autonoetic_types::agent::ThinkingEffort;
            match self.provider.capabilities.reasoning {
                crate::llm::provider::ReasoningStyle::None => {}
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

        // Prompt caching: send a stable key so repeated turns in a session
        // reuse cached prompt-prefix tokens. OpenRouter and OpenAI both accept
        // top-level `prompt_cache_key`; other OpenAI-compatible providers
        // ignore unknown fields, so gate on the providers known to honor it.
        if let Some(ref key) = req.prompt_cache_key {
            if matches!(
                self.provider.capabilities.reasoning,
                crate::llm::provider::ReasoningStyle::OpenRouterUnified
                    | crate::llm::provider::ReasoningStyle::OpenAiEffort
            ) {
                body["prompt_cache_key"] = json!(key);
            }
        }

        body
    }
}

#[async_trait::async_trait]
impl LlmDriver for OpenAiDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let body = self.build_body(req, false);

        // Debug log the request body (trace level for full body)
        tracing::debug!(target: "llm::openai", model=%self.provider.model, "Sending LLM request");
        if tracing::enabled!(tracing::Level::TRACE) {
            if let Ok(pretty) = serde_json::to_string_pretty(&body) {
                tracing::trace!(target: "llm::openai", "Full request body:\n{}", pretty);
            }
        }

        const MAX_RETRIES: u32 = 3;
        for attempt in 0..=MAX_RETRIES {
            let builder = self.apply_auth(
                self.client
                    .post(&self.provider.base_url)
                    .header("Content-Type", "application/json")
                    .json(&body),
            );

            let response = match builder.send().await {
                Ok(r) => r,
                Err(e) if crate::llm::is_transient_connection_error(&e) && attempt < MAX_RETRIES => {
                    let wait_ms = crate::llm::connection_retry_backoff_ms(attempt);
                    tracing::warn!(
                        attempt,
                        wait_ms,
                        error = %e,
                        "LLM connection error, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = response.status();

            if status.as_u16() == 429 || status.as_u16() == 529 {
                if attempt < MAX_RETRIES {
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
                anyhow::bail!("OpenAI API rate limited after {} retries", MAX_RETRIES);
            }

            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                if crate::llm::is_context_overflow_error(status.as_u16(), &text) {
                    anyhow::bail!(
                        "context_overflow: provider=openai status={} detail={}", status, text
                    );
                }
                tracing::warn!(
                    target: "llm::openai",
                    status = %status,
                    model = %self.provider.model,
                    "LLM API error response"
                );
                anyhow::bail!("OpenAI API error {}: {}", status, text);
            }

            let j: serde_json::Value = response.json().await?;
            return Ok(parse_response(&j));
        }
        anyhow::bail!("Max retries exceeded");
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

        const MAX_RETRIES: u32 = 3;
        for attempt in 0..=MAX_RETRIES {
            let builder = self.apply_auth(
                self.client
                    .post(&self.provider.base_url)
                    .header("Content-Type", "application/json")
                    .header("Accept", "text/event-stream")
                    .json(&body),
            );

            let response = match builder.send().await {
                Ok(r) => r,
                Err(e) if crate::llm::is_transient_connection_error(&e) && attempt < MAX_RETRIES => {
                    let wait_ms = crate::llm::connection_retry_backoff_ms(attempt);
                    tracing::warn!(
                        attempt,
                        wait_ms,
                        error = %e,
                        "LLM stream connection error, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                anyhow::bail!("OpenAI stream error {}: {}", status, text);
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
                let chunk = chunk?;
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
        anyhow::bail!("Max connection retries exceeded");
    }
}

/// Parse a non-streaming JSON response body.
fn parse_response(j: &serde_json::Value) -> CompletionResponse {
    let choice = &j["choices"][0];
    let text = extract_text_content(&choice["message"]["content"]);
    let reasoning_content = extract_reasoning_content(&choice["message"]);

    let tool_calls = choice["message"]["tool_calls"]
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

    let stop_reason = parse_stop_reason(choice["finish_reason"].as_str().unwrap_or(""));

    let reasoning_details = extract_reasoning_details(&choice["message"]);

    let usage = parse_usage(&j["usage"]);

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
        };
        let body = driver.build_body(&req, false);
        assert_eq!(body["messages"][0]["reasoning_content"], "plain");
    }

    #[test]
    fn prompt_cache_key_emitted_for_openrouter() {
        let driver = driver_with("deepseek/deepseek-v4-flash", ReasoningStyle::OpenRouterUnified);
        let mut req = req_with_thinking("deepseek/deepseek-v4-flash", ThinkingEffort::Low, None);
        req.prompt_cache_key = Some("session-abc".to_string());
        let body = driver.build_body(&req, false);
        assert_eq!(body["prompt_cache_key"], "session-abc");
    }

    #[test]
    fn prompt_cache_key_omitted_for_plain_provider() {
        // A provider with ReasoningStyle::None (e.g. groq) doesn't get the key.
        let driver = driver_with("llama-3.1-70b", ReasoningStyle::None);
        let mut req = req_with_thinking("llama-3.1-70b", ThinkingEffort::Low, None);
        req.prompt_cache_key = Some("session-abc".to_string());
        let body = driver.build_body(&req, false);
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn stream_request_includes_usage_option() {
        let driver = driver_with("gpt-4o", ReasoningStyle::None);
        let req = CompletionRequest::simple("gpt-4o", vec![Message::user("hi")]);
        let body = driver.build_body(&req, true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }
}
