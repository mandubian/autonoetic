//! Google Gemini generateContent API driver.
//!
//! Uses the unique Gemini content/parts format with functionDeclarations.
//! All credential/endpoint resolution is done by `provider::resolve()`.

use super::{
    CompletionRequest, CompletionResponse, LlmDriver, Role, StopReason, TokenUsage, ToolCall,
};
use crate::llm::provider::{AuthStrategy, ResolvedProvider};
use reqwest::Client;
use serde_json::json;

pub struct GeminiDriver {
    client: Client,
    provider: ResolvedProvider,
}

impl GeminiDriver {
    pub fn new(client: Client, provider: ResolvedProvider) -> Self {
        Self { client, provider }
    }

    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.provider.auth {
            AuthStrategy::GoogleApiKey(key) => builder.header("x-goog-api-key", key),
            _ => builder,
        }
    }

    /// Gemini embeds the model name in the URL path.
    fn url(&self) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.provider.base_url, self.provider.model
        )
    }

    fn build_body(&self, req: &CompletionRequest) -> serde_json::Value {
        let mut system_instruction = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();

        for m in &req.messages {
            match m.role {
                Role::System => {
                    system_instruction = Some(json!({ "parts": [{ "text": m.content }] }));
                }
                Role::User => {
                    if let Some(ref id) = m.tool_call_id {
                        contents.push(json!({
                            "role": "user",
                            "parts": [{ "functionResponse": { "name": id, "response": { "content": m.content } } }]
                        }));
                    } else {
                        contents.push(json!({ "role": "user", "parts": [{ "text": m.content }] }));
                    }
                }
                Role::Assistant => {
                    if !m.tool_calls.is_empty() {
                        let parts: Vec<serde_json::Value> = m
                            .tool_calls
                            .iter()
                            .map(|tc| {
                                let args: serde_json::Value =
                                    serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                                json!({ "functionCall": { "name": tc.name, "args": args } })
                            })
                            .collect();
                        contents.push(json!({ "role": "model", "parts": parts }));
                    } else {
                        contents.push(json!({ "role": "model", "parts": [{ "text": m.content }] }));
                    }
                }
                Role::Tool => {}
            }
        }

        let mut body = json!({ "contents": contents });
        if let Some(sys) = system_instruction {
            body["systemInstruction"] = sys;
        }

        let mut gen: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        if let Some(max) = req.max_tokens.or(self.provider.max_tokens) {
            gen.insert("maxOutputTokens".into(), json!(max));
        }
        let t = req.temperature.or(self.provider.temperature);
        if let Some(t) = t {
            if t > 0.0 {
                gen.insert("temperature".into(), json!(t));
            }
        }
        if !gen.is_empty() {
            body["generationConfig"] = serde_json::Value::Object(gen);
        }

        if !req.tools.is_empty() {
            body["tools"] = json!([{
                "functionDeclarations": req.tools.iter().map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    // Gemini's functionDeclaration schema is *not* full JSON Schema; in particular
                    // it rejects fields like `additionalProperties` (400 INVALID_ARGUMENT).
                    "parameters": sanitize_schema_for_gemini(&t.input_schema),
                })).collect::<Vec<_>>()
            }]);
        }

        if let Some(ref thinking) = req.thinking {
            if model_is_gemma(&self.provider.model)
                && !matches!(
                    thinking.effort,
                    autonoetic_types::agent::ThinkingEffort::Low
                        | autonoetic_types::agent::ThinkingEffort::Medium
                        | autonoetic_types::agent::ThinkingEffort::High
                        | autonoetic_types::agent::ThinkingEffort::XHigh
                )
            {
            } else if model_is_gemma(&self.provider.model) {
                if let Some(ref mut sys) = body.get_mut("systemInstruction") {
                    if let Some(parts) = sys.get_mut("parts").and_then(|p| p.as_array_mut()) {
                        if let Some(first) = parts.get_mut(0).and_then(|p| p.get_mut("text")) {
                            if let Some(text) = first.as_str() {
                                *first = json!({ "text": format!("<|think|>\n{}", text) });
                            }
                        }
                    }
                } else {
                    body["systemInstruction"] = json!({
                        "parts": [{ "text": "<|think|>" }]
                    });
                }
            }
        }

        body
    }
}

fn sanitize_schema_for_gemini(schema: &serde_json::Value) -> serde_json::Value {
    fn sanitize(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, val) in map {
                    if k == "additionalProperties" {
                        continue;
                    }
                    out.insert(k.clone(), sanitize(val));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(sanitize).collect())
            }
            other => other.clone(),
        }
    }

    sanitize(schema)
}

fn model_is_gemma(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("gemma") || m.contains("gemini-2") || m.contains("gemini-1")
}

#[async_trait::async_trait]
impl LlmDriver for GeminiDriver {
    async fn complete(&self, req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let body = self.build_body(req);
        let url = self.url();

        const MAX_RETRIES: u32 = 3;
        for attempt in 0..=MAX_RETRIES {
            let builder = self.apply_auth(
                self.client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .json(&body),
            );
            let response = builder.send().await?;
            let status = response.status().as_u16();

            // Check context overflow *before* rate-limit retry, because
            // Gemini sometimes overflows as HTTP 429 with RESOURCE_EXHAUSTED.
            if !reqwest::StatusCode::from_u16(status)
                .map(|s| s.is_success())
                .unwrap_or(false)
            {
                let text = response.text().await.unwrap_or_default();
                if crate::llm::is_context_overflow_error(status, &text) {
                    anyhow::bail!(
                        "context_overflow: provider=gemini status={} detail={}", status, text
                    );
                }
                if status == 429 {
                    if attempt < MAX_RETRIES {
                        let wait_ms = (attempt + 1) as u64 * 2000;
                        tracing::warn!(status, attempt, wait_ms, "Gemini rate limited, retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                    anyhow::bail!("Gemini rate limited after {} retries", MAX_RETRIES);
                }
                anyhow::bail!("Gemini API error {}: {}", status, text);
            }

            let j: serde_json::Value = response.json().await?;
            return Ok(parse_response(&j));
        }
        anyhow::bail!("Max retries exceeded");
    }
}

fn parse_response(j: &serde_json::Value) -> CompletionResponse {
    let parts = &j["candidates"][0]["content"]["parts"];
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(parts_arr) = parts.as_array() {
        for part in parts_arr {
            if let Some(t) = part["text"].as_str() {
                text.push_str(t);
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or("").to_string();
                let arguments = serde_json::to_string(&fc["args"]).unwrap_or_default();
                tool_calls.push(ToolCall {
                    id: format!("gemini-{}", name),
                    name,
                    arguments,
                });
            }
        }
    }

    let stop_reason = match j["candidates"][0]["finishReason"].as_str().unwrap_or("") {
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        _ if !tool_calls.is_empty() => StopReason::ToolUse,
        other => StopReason::Other(other.to_string()),
    };
    let usage = TokenUsage {
        input_tokens: j["usageMetadata"]["promptTokenCount"].as_u64().unwrap_or(0),
        output_tokens: j["usageMetadata"]["candidatesTokenCount"]
            .as_u64()
            .unwrap_or(0),
        // Gemini reports thoughtsTokenCount for thinking models; cached content
        // tokens under cachedContentTokenCount.
        reasoning_tokens: j["usageMetadata"]["thoughtsTokenCount"].as_u64().unwrap_or(0),
        cached_tokens: j["usageMetadata"]["cachedContentTokenCount"]
            .as_u64()
            .unwrap_or(0),
    };

    CompletionResponse {
        text,
        tool_calls,
        reasoning_content: None,
        reasoning_details: None,
        stop_reason,
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_schema_for_gemini;
    use serde_json::json;

    #[test]
    fn sanitize_schema_strips_additional_properties_recursively() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "a": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "properties": { "b": { "type": "string" } }
                }
            },
            "anyOf": [
                { "type": "object", "additionalProperties": true }
            ]
        });

        let sanitized = sanitize_schema_for_gemini(&schema);
        assert!(sanitized.get("additionalProperties").is_none());
        assert!(sanitized["properties"]["a"]
            .get("additionalProperties")
            .is_none());
        assert!(sanitized["anyOf"][0].get("additionalProperties").is_none());
    }
}
