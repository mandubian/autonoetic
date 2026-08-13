//! Golden tests for LLM provider payload translation.
//!
//! These tests verify the exact shape of JSON sent to each provider's API.
//! They do NOT make network calls; they exercise the payload-building functions directly.

#[cfg(test)]
mod tests {
    use crate::llm::{CompletionRequest, Message, Role, ToolCall, ToolDefinition};
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn search_tool() -> ToolDefinition {
        ToolDefinition {
            name: "search".to_string(),
            description: "Search the web for a query".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        }
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call_abc123".to_string(),
            name: "search".to_string(),
            arguments: r#"{"query":"rust lifetimes"}"#.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // OpenAI golden tests
    // -----------------------------------------------------------------------

    mod openai {
        use super::*;

        /// Build the OpenAI payload the same way OpenAiDriver.build_body() does.
        fn build_payload(req: &CompletionRequest) -> serde_json::Value {
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
                                "id": tc.id, "type": "function",
                                "function": { "name": tc.name, "arguments": tc.arguments }
                            }))
                            .collect::<Vec<_>>());
                    }
                    if let Some(ref id) = m.tool_call_id {
                        msg["tool_call_id"] = json!(id);
                    }
                    msg
                })
                .collect();

            let mut body = json!({ "model": req.model, "messages": messages });

            if !req.tools.is_empty() {
                body["tools"] = json!(req.tools.iter().map(|t| json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.description, "parameters": t.input_schema }
                })).collect::<Vec<_>>());
                body["tool_choice"] = json!("auto");
            }
            body
        }

        #[test]
        fn test_simple_user_message() {
            let req = CompletionRequest::simple(
                "gpt-4o",
                vec![
                    Message::system("You are a helpful assistant"),
                    Message::user("Hello"),
                ],
            );
            let body = build_payload(&req);
            assert_eq!(body["model"], "gpt-4o");
            assert_eq!(body["messages"][0]["role"], "system");
            assert_eq!(
                body["messages"][0]["content"],
                "You are a helpful assistant"
            );
            assert_eq!(body["messages"][1]["role"], "user");
            assert_eq!(body["messages"][1]["content"], "Hello");
            assert!(body.get("tools").is_none());
        }

        #[test]
        fn test_tool_definition_serialization() {
            let req = CompletionRequest {
                model: "gpt-4o".to_string(),
                messages: vec![Message::user("search for rust")],
                tools: vec![search_tool()],
                max_tokens: None,
                temperature: None,
                metadata: None,
                thinking: None,
                prompt_cache_key: None,
                system_cache_prefix_bytes: None,
            };
            let body = build_payload(&req);
            assert_eq!(body["tools"][0]["type"], "function");
            assert_eq!(body["tools"][0]["function"]["name"], "search");
            assert_eq!(body["tool_choice"], "auto");
            assert_eq!(
                body["tools"][0]["function"]["parameters"]["required"][0],
                "query"
            );
        }

        #[test]
        fn test_assistant_tool_call_turn() {
            // Golden: the shape of an assistant message that contains a tool call
            let mut assistant_msg = Message::assistant(""); // no text content
            assistant_msg.tool_calls = vec![tool_call()];

            let req = CompletionRequest::simple(
                "gpt-4o",
                vec![Message::user("Search for something"), assistant_msg],
            );
            let body = build_payload(&req);
            let asst = &body["messages"][1];
            assert_eq!(asst["role"], "assistant");
            assert_eq!(asst["tool_calls"][0]["id"], "call_abc123");
            assert_eq!(asst["tool_calls"][0]["type"], "function");
            assert_eq!(asst["tool_calls"][0]["function"]["name"], "search");
            assert_eq!(
                asst["tool_calls"][0]["function"]["arguments"],
                r#"{"query":"rust lifetimes"}"#
            );
        }

        #[test]
        fn test_tool_result_turn() {
            // Golden: the shape of a tool result (role="tool") message
            let result_msg = Message::tool_result(
                "call_abc123",
                "search",
                "Rust lifetimes control ownership scopes.",
            );
            let req = CompletionRequest::simple("gpt-4o", vec![result_msg]);
            let body = build_payload(&req);
            assert_eq!(body["messages"][0]["role"], "tool");
            assert_eq!(body["messages"][0]["tool_call_id"], "call_abc123");
            assert_eq!(
                body["messages"][0]["content"],
                "Rust lifetimes control ownership scopes."
            );
        }

        #[test]
        fn test_parse_tool_call_response() {
            // Golden: parse a response containing a tool_calls array
            let raw = json!({
                "choices": [{
                    "finish_reason": "tool_calls",
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_abc123",
                            "type": "function",
                            "function": {
                                "name": "search",
                                "arguments": "{\"query\": \"rust lifetimes\"}"
                            }
                        }]
                    }
                }],
                "usage": { "prompt_tokens": 50, "completion_tokens": 20 }
            });

            let tool_calls = raw["choices"][0]["message"]["tool_calls"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tc| {
                    Some(ToolCall {
                        id: tc["id"].as_str()?.to_string(),
                        name: tc["function"]["name"].as_str()?.to_string(),
                        arguments: tc["function"]["arguments"]
                            .as_str()
                            .unwrap_or("{}")
                            .to_string(),
                    })
                })
                .collect::<Vec<_>>();

            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call_abc123");
            assert_eq!(tool_calls[0].name, "search");

            let finish = raw["choices"][0]["finish_reason"].as_str().unwrap();
            assert_eq!(finish, "tool_calls");
        }
    }

    // -----------------------------------------------------------------------
    // Anthropic golden tests
    // -----------------------------------------------------------------------

    mod anthropic {
        use super::*;

        fn build_payload(req: &CompletionRequest) -> serde_json::Value {
            let mut system_text = String::new();
            let mut messages: Vec<serde_json::Value> = Vec::new();

            for m in &req.messages {
                match m.role {
                    Role::System => {
                        system_text.push_str(&m.content);
                    }
                    Role::User => {
                        if let Some(ref id) = m.tool_call_id {
                            messages.push(json!({
                                "role": "user",
                                "content": [{ "type": "tool_result", "tool_use_id": id, "content": m.content }]
                            }));
                        } else {
                            messages.push(json!({ "role": "user", "content": m.content }));
                        }
                    }
                    Role::Assistant => {
                        if !m.tool_calls.is_empty() {
                            let mut content: Vec<serde_json::Value> = Vec::new();
                            if !m.content.is_empty() {
                                content.push(json!({ "type": "text", "text": m.content }));
                            }
                            for tc in &m.tool_calls {
                                let input: serde_json::Value =
                                    serde_json::from_str(&tc.arguments).unwrap_or(json!({}));
                                content.push(json!({ "type": "tool_use", "id": tc.id, "name": tc.name, "input": input }));
                            }
                            messages.push(json!({ "role": "assistant", "content": content }));
                        } else {
                            messages.push(json!({ "role": "assistant", "content": m.content }));
                        }
                    }
                    Role::Tool => {
                        if let Some(ref id) = m.tool_call_id {
                            messages.push(json!({
                                "role": "user",
                                "content": [{ "type": "tool_result", "tool_use_id": id, "content": m.content }]
                            }));
                        }
                    }
                }
            }

            let mut body = json!({ "model": "claude-3-5-sonnet-20241022", "max_tokens": 4096, "messages": messages });
            if !system_text.is_empty() {
                body["system"] = json!(system_text.trim());
            }
            if !req.tools.is_empty() {
                body["tools"] =
                    json!(req.tools.iter().map(|t| json!({
                    "name": t.name, "description": t.description, "input_schema": t.input_schema
                })).collect::<Vec<_>>());
            }
            body
        }

        #[test]
        fn test_system_extracted_to_top_level() {
            let req = CompletionRequest::simple(
                "claude-3-5-sonnet-20241022",
                vec![
                    Message::system("You are a wise assistant"),
                    Message::user("Hello"),
                ],
            );
            let body = build_payload(&req);
            // System must NOT be in messages array
            assert_eq!(body["system"], "You are a wise assistant");
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
            assert_eq!(body["messages"][0]["role"], "user");
        }

        #[test]
        fn test_tool_definition_format() {
            let req = CompletionRequest {
                model: "claude-3-5-sonnet-20241022".to_string(),
                messages: vec![Message::user("use a tool")],
                tools: vec![search_tool()],
                max_tokens: None,
                temperature: None,
                metadata: None,
                thinking: None,
                prompt_cache_key: None,
                system_cache_prefix_bytes: None,
            };
            let body = build_payload(&req);
            // Anthropic uses "input_schema", NOT "parameters"
            assert_eq!(body["tools"][0]["name"], "search");
            assert!(body["tools"][0].get("parameters").is_none());
            assert!(body["tools"][0]["input_schema"]["properties"]
                .get("query")
                .is_some());
        }

        #[test]
        fn test_tool_use_block_in_assistant_turn() {
            let mut asst = Message::assistant("");
            asst.tool_calls = vec![tool_call()];

            let req = CompletionRequest::simple(
                "claude-3-5-sonnet-20241022",
                vec![Message::user("search something"), asst],
            );
            let body = build_payload(&req);
            let asst_content = &body["messages"][1]["content"];
            assert_eq!(asst_content[0]["type"], "tool_use");
            assert_eq!(asst_content[0]["id"], "call_abc123");
            assert_eq!(asst_content[0]["name"], "search");
            assert_eq!(asst_content[0]["input"]["query"], "rust lifetimes");
        }

        #[test]
        fn test_tool_result_as_user_content_block() {
            let tool_result =
                Message::tool_result("call_abc123", "search", "Lifetimes are scopes.");
            let req = CompletionRequest::simple("claude-3-5-sonnet-20241022", vec![tool_result]);
            let body = build_payload(&req);

            // Anthropic expects tool results wrapped in user message content blocks
            assert_eq!(body["messages"][0]["role"], "user");
            let content = &body["messages"][0]["content"][0];
            assert_eq!(content["type"], "tool_result");
            assert_eq!(content["tool_use_id"], "call_abc123");
            assert_eq!(content["content"], "Lifetimes are scopes.");
        }

        #[test]
        fn test_parse_tool_use_response() {
            let raw = json!({
                "stop_reason": "tool_use",
                "content": [{
                    "type": "tool_use",
                    "id": "call_abc123",
                    "name": "search",
                    "input": { "query": "rust lifetimes" }
                }],
                "usage": { "input_tokens": 80, "output_tokens": 30 }
            });

            let tool_calls: Vec<ToolCall> = raw["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|b| {
                    if b["type"].as_str() == Some("tool_use") {
                        Some(ToolCall {
                            id: b["id"].as_str()?.to_string(),
                            name: b["name"].as_str()?.to_string(),
                            arguments: serde_json::to_string(&b["input"]).unwrap_or_default(),
                        })
                    } else {
                        None
                    }
                })
                .collect();

            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "search");
            let args: serde_json::Value = serde_json::from_str(&tool_calls[0].arguments).unwrap();
            assert_eq!(args["query"], "rust lifetimes");
        }
    }

    // -----------------------------------------------------------------------
    // Gemini golden tests
    // -----------------------------------------------------------------------

    mod gemini {
        use super::*;

        fn build_payload(req: &CompletionRequest) -> serde_json::Value {
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
                            contents
                                .push(json!({ "role": "user", "parts": [{ "text": m.content }] }));
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
                            contents
                                .push(json!({ "role": "model", "parts": [{ "text": m.content }] }));
                        }
                    }
                    Role::Tool => {
                        if let Some(ref id) = m.tool_call_id {
                            contents.push(json!({
                                "role": "user",
                                "parts": [{ "functionResponse": { "name": id, "response": { "content": m.content } } }]
                            }));
                        }
                    }
                }
            }

            let mut body = json!({ "contents": contents });
            if let Some(sys) = system_instruction {
                body["systemInstruction"] = sys;
            }
            if !req.tools.is_empty() {
                body["tools"] = json!([{
                    "functionDeclarations": req.tools.iter().map(|t| json!({
                        "name": t.name, "description": t.description, "parameters": t.input_schema
                    })).collect::<Vec<_>>()
                }]);
            }
            body
        }

        #[test]
        fn test_user_message_in_parts() {
            let req = CompletionRequest::simple(
                "gemini-2.5-pro",
                vec![Message::system("Be helpful"), Message::user("Hello")],
            );
            let body = build_payload(&req);
            // System becomes systemInstruction, not a content entry
            assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be helpful");
            assert_eq!(body["contents"].as_array().unwrap().len(), 1);
            assert_eq!(body["contents"][0]["role"], "user");
            assert_eq!(body["contents"][0]["parts"][0]["text"], "Hello");
        }

        #[test]
        fn test_function_declarations_format() {
            let req = CompletionRequest {
                model: "gemini-2.5-pro".to_string(),
                messages: vec![Message::user("search")],
                tools: vec![search_tool()],
                max_tokens: None,
                temperature: None,
                metadata: None,
                thinking: None,
                prompt_cache_key: None,
                system_cache_prefix_bytes: None,
            };
            let body = build_payload(&req);
            // Gemini wraps tools in functionDeclarations inside a tools array
            let decls = &body["tools"][0]["functionDeclarations"];
            assert_eq!(decls[0]["name"], "search");
            assert!(decls[0]["parameters"]["properties"].get("query").is_some());
        }

        #[test]
        fn test_function_call_in_model_turn() {
            let mut asst = Message::assistant("");
            asst.tool_calls = vec![tool_call()];

            let req = CompletionRequest::simple(
                "gemini-2.5-pro",
                vec![Message::user("search something"), asst],
            );
            let body = build_payload(&req);
            let model_turn = &body["contents"][1];
            assert_eq!(model_turn["role"], "model");
            assert_eq!(model_turn["parts"][0]["functionCall"]["name"], "search");
            assert_eq!(
                model_turn["parts"][0]["functionCall"]["args"]["query"],
                "rust lifetimes"
            );
        }

        #[test]
        fn test_function_response_in_user_turn() {
            let tool_result =
                Message::tool_result("call_abc123", "search", "Lifetimes annotate scopes.");
            let req = CompletionRequest::simple("gemini-2.5-pro", vec![tool_result]);
            let body = build_payload(&req);
            // Gemini tool results go in user parts as functionResponse
            let part = &body["contents"][0]["parts"][0]["functionResponse"];
            assert_eq!(body["contents"][0]["role"], "user");
            assert_eq!(part["response"]["content"], "Lifetimes annotate scopes.");
        }

        #[test]
        fn test_parse_function_call_response() {
            let raw = json!({
                "candidates": [{
                    "finishReason": "STOP",
                    "content": {
                        "role": "model",
                        "parts": [{
                            "functionCall": {
                                "name": "search",
                                "args": { "query": "rust lifetimes" }
                            }
                        }]
                    }
                }],
                "usageMetadata": { "promptTokenCount": 60, "candidatesTokenCount": 15 }
            });

            let parts = &raw["candidates"][0]["content"]["parts"];
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            for part in parts.as_array().unwrap() {
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

            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "search");
            let args: serde_json::Value = serde_json::from_str(&tool_calls[0].arguments).unwrap();
            assert_eq!(args["query"], "rust lifetimes");
        }
    }

    // -----------------------------------------------------------------------
    // is_context_overflow_error — cross-provider error body recognition
    // -----------------------------------------------------------------------

    mod context_overflow {
        use crate::llm::is_context_overflow_error;

        #[test]
        fn rejects_status_zero() {
            // Status 0 means we never reached the server; never treat as overflow.
            assert!(!is_context_overflow_error(
                0,
                r#"{"error":{"code":"context_length_exceeded"}}"#
            ));
        }

        #[test]
        fn openai_cloud_string_code() {
            let body = r#"{"error":{"message":"too long","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
            assert!(is_context_overflow_error(400, body));
        }

        #[test]
        fn openai_cloud_unrelated_400_is_not_overflow() {
            let body = r#"{"error":{"message":"missing api key","type":"invalid_request_error","code":"invalid_api_key"}}"#;
            assert!(!is_context_overflow_error(401, body));
        }

        #[test]
        fn llama_cpp_numeric_code_and_exceed_context_size_type() {
            // Real body observed from llama.cpp / lmstudio when prompt exceeds n_ctx.
            // Note: `code` is a NUMBER (400), not a string — the previous
            // implementation missed this because it required a string code.
            let body = r#"{"error":{"code":400,"message":"request (115537 tokens) exceeds the available context size (114688 tokens),try increasing it","type":"exceed_context_size_error","n_prompt_tokens":115537,"n_ctx":114688}}"#;
            assert!(is_context_overflow_error(400, body));
        }

        #[test]
        fn message_text_fallback_when_no_structured_signal() {
            // Some OpenAI-compatible servers omit `code`/`type` but still
            // surface the overflow in the message.
            let body = r#"{"error":{"message":"This model's maximum context length is 4096 tokens"}}"#;
            assert!(is_context_overflow_error(400, body));
        }

        #[test]
        fn message_text_exceeds_context_phrase() {
            let body = r#"{"error":{"message":"request exceeds the context length supported by this model"}}"#;
            assert!(is_context_overflow_error(400, body));
        }

        #[test]
        fn context_size_has_been_exceeded_500() {
            // Observed from a local OpenAI-compatible server: HTTP 500 with a
            // non-standard "Context size has been exceeded." message and a
            // generic server_error type. Previously unrecognized → terminal
            // failure instead of an aggressive-governor retry.
            let body = r#"{"error":{"code":500,"message":"Context size has been exceeded.","type":"server_error"}}"#;
            assert!(is_context_overflow_error(500, body));
        }

        #[test]
        fn unrelated_context_error_without_overflow_verb_is_not_overflow() {
            // Guard against the general "context" catch over-matching: an error
            // mentioning context but no overflow verb must NOT be treated as
            // overflow.
            let body = r#"{"error":{"message":"invalid context id provided","type":"invalid_request_error"}}"#;
            assert!(!is_context_overflow_error(400, body));
        }

        #[test]
        fn context_deadline_exceeded_timeout_is_not_overflow() {
            // "context deadline exceeded" is a timeout/cancellation (Go/gRPC),
            // NOT a context-size overflow. It has "context" + "exceed" but no
            // size/length/window/token hint, so it must NOT route into
            // overflow recovery (trimming context wouldn't help a timeout).
            let body = r#"{"error":{"message":"context deadline exceeded","type":"server_error"}}"#;
            assert!(!is_context_overflow_error(500, body));
        }

        #[test]
        fn n_prompt_tokens_and_n_ctx_keys_alone_are_enough() {
            // Defensive: if the server reports both counters, it's an overflow
            // even if the message wording is unusual.
            let body =
                r#"{"error":{"message":"boom","n_prompt_tokens":200000,"n_ctx":131072}}"#;
            assert!(is_context_overflow_error(400, body));
        }

        #[test]
        fn anthropic_max_context_window_reached() {
            assert!(is_context_overflow_error(
                400,
                "max_context_window_reached: input too long"
            ));
        }

        #[test]
        fn gemini_resource_exhausted_with_context() {
            assert!(is_context_overflow_error(
                429,
                r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"context too long"}}"#
            ));
        }

        #[test]
        fn gemini_resource_exhausted_without_context_is_not_overflow() {
            // RESOURCE_EXHAUSTED without "context" is a rate-limit, not overflow.
            assert!(!is_context_overflow_error(
                429,
                r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"rate limit hit"}}"#
            ));
        }

        #[test]
        fn safety_filter_treated_as_overflow_like() {
            assert!(is_context_overflow_error(
                400,
                r#"{"candidates":[{"finishReason":"SAFETY"}]}"#
            ));
        }

        #[test]
        fn non_json_body_returns_false_when_no_known_marker() {
            assert!(!is_context_overflow_error(500, "internal server error"));
        }
    }

    // -----------------------------------------------------------------------
    // Per-request timeout resolution — env vs preset vs config vs default
    // -----------------------------------------------------------------------
    //
    // Exercises the pure core rather than `build_driver`, which reads
    // process env and a `OnceLock`: both are global, so a test touching them
    // would order-depend on every other test in the binary.
    mod request_timeout_resolution {
        use crate::llm::{resolve_request_timeout_secs, DEFAULT_REQUEST_TIMEOUT_SECS};

        #[test]
        fn unset_everywhere_is_the_default() {
            assert_eq!(
                resolve_request_timeout_secs(None, None, None),
                DEFAULT_REQUEST_TIMEOUT_SECS
            );
        }

        #[test]
        fn configured_value_applies_when_env_and_preset_are_absent() {
            assert_eq!(resolve_request_timeout_secs(None, None, Some(600)), 600);
        }

        /// The env var stays an ad-hoc escape hatch, so it must win over a
        /// committed config value rather than the other way round.
        #[test]
        fn env_overrides_configured_value() {
            assert_eq!(resolve_request_timeout_secs(Some("300"), None, Some(600)), 300);
        }

        /// #1045: the preset-level value sits between env and the gateway
        /// default — a `coding` preset can outlast the global budget without
        /// an env tweak.
        #[test]
        fn preset_applies_when_env_is_absent() {
            assert_eq!(resolve_request_timeout_secs(None, Some(300), None), 300);
        }

        #[test]
        fn preset_beats_gateway_default() {
            assert_eq!(resolve_request_timeout_secs(None, Some(300), Some(600)), 300);
        }

        #[test]
        fn env_beats_preset() {
            assert_eq!(
                resolve_request_timeout_secs(Some("900"), Some(300), Some(600)),
                900
            );
        }

        /// A sub-floor preset value is a misconfiguration, not an intent —
        /// fall through to the gateway default rather than clamping to it.
        #[test]
        fn sub_floor_preset_falls_through_to_configured() {
            assert_eq!(resolve_request_timeout_secs(None, Some(4), Some(600)), 600);
        }

        #[test]
        fn surrounding_whitespace_in_env_is_tolerated() {
            assert_eq!(resolve_request_timeout_secs(Some("  300 "), None, None), 300);
        }

        /// A malformed or sub-floor env value must fall through to config, not
        /// silently discard it — otherwise a stray export erases the operator's
        /// configured budget.
        #[test]
        fn unusable_env_falls_through_to_configured_value() {
            for env in ["not-a-number", "0", "4", ""] {
                assert_eq!(
                    resolve_request_timeout_secs(Some(env), None, Some(600)),
                    600,
                    "env {env:?} should fall through to config"
                );
            }
        }

        #[test]
        fn sub_floor_configured_value_falls_back_to_default() {
            assert_eq!(
                resolve_request_timeout_secs(None, None, Some(4)),
                DEFAULT_REQUEST_TIMEOUT_SECS
            );
        }

        #[test]
        fn floor_value_is_accepted() {
            assert_eq!(resolve_request_timeout_secs(None, None, Some(5)), 5);
            assert_eq!(resolve_request_timeout_secs(Some("5"), None, None), 5);
        }
    }

    // -----------------------------------------------------------------------
    // Fail-fast retry policy — retry_wait_decision / default timeout
    // -----------------------------------------------------------------------
    mod retry_policy {
        use crate::llm::{
            retry_wait_decision, DEFAULT_REQUEST_TIMEOUT_SECS, MAX_CONNECTION_RETRIES,
            MAX_TIMEOUT_RETRIES,
        };
        use std::time::Duration;

        const DEADLINE: Duration = Duration::from_secs(240);

        #[test]
        fn non_transient_never_retries() {
            assert_eq!(
                retry_wait_decision(false, false, 0, Duration::ZERO, DEADLINE),
                None
            );
        }

        #[test]
        fn timeout_retries_at_most_once() {
            assert_eq!(MAX_TIMEOUT_RETRIES, 1);
            // attempt 0 retries…
            assert!(retry_wait_decision(true, true, 0, Duration::ZERO, DEADLINE).is_some());
            // …attempt 1 stops.
            assert_eq!(
                retry_wait_decision(true, true, 1, Duration::ZERO, DEADLINE),
                None
            );
        }

        #[test]
        fn fast_connection_error_retries_up_to_max() {
            assert_eq!(MAX_CONNECTION_RETRIES, 3);
            for attempt in 0..MAX_CONNECTION_RETRIES {
                assert!(
                    retry_wait_decision(true, false, attempt, Duration::ZERO, DEADLINE).is_some(),
                    "attempt {attempt} should retry"
                );
            }
            assert_eq!(
                retry_wait_decision(true, false, MAX_CONNECTION_RETRIES, Duration::ZERO, DEADLINE),
                None
            );
        }

        #[test]
        fn wall_clock_deadline_stops_retries_even_with_budget() {
            // attempt 0 (budget remains) but elapsed past the deadline → stop.
            assert_eq!(
                retry_wait_decision(true, false, 0, Duration::from_secs(241), DEADLINE),
                None
            );
            assert_eq!(
                retry_wait_decision(true, true, 0, Duration::from_secs(241), DEADLINE),
                None
            );
        }

        #[test]
        fn default_timeout_is_two_minutes() {
            assert_eq!(DEFAULT_REQUEST_TIMEOUT_SECS, 120);
        }

        #[test]
        fn backoff_that_would_cross_deadline_stops_now() {
            // attempt 2 still has connection budget (cap 3) and elapsed (239.5s)
            // is under the 240s deadline — but the attempt-2 backoff (3000ms)
            // would push us past it, so we must NOT sleep-then-retry late.
            assert_eq!(
                retry_wait_decision(true, false, 2, Duration::from_millis(239_500), DEADLINE),
                None
            );
            // Comfortably under the deadline: attempt 1 at 10s elapsed retries.
            assert!(
                retry_wait_decision(true, false, 1, Duration::from_secs(10), DEADLINE).is_some()
            );
        }

        /// #1043: the first connection/timeout backoff must be non-zero —
        /// `attempt * 1000` made the only retry a timeout ever gets fire
        /// instantly, re-sending the same heavy request the moment the
        /// previous attempt gave up. The shape mirrors the 429 backoff.
        #[test]
        fn first_connection_backoff_is_non_zero() {
            use crate::llm::connection_retry_backoff_ms;
            assert_eq!(connection_retry_backoff_ms(0), 1000);
            assert_eq!(connection_retry_backoff_ms(1), 2000);
            assert_eq!(connection_retry_backoff_ms(2), 3000);
        }

        /// #1043: the retry deadline is two full attempts PLUS the backoff
        /// budget between retries — the old `2 × timeout` was denominated in
        /// the same unit the attempts consume, so the backoff could never
        /// fit and the retry degenerated to an instant duplicate.
        #[test]
        fn retry_deadline_leaves_room_for_backoffs() {
            use crate::llm::{retry_backoff_budget, retry_deadline};
            let timeout = Duration::from_secs(120);
            let deadline = retry_deadline(timeout);
            assert_eq!(
                deadline,
                timeout * 2 + retry_backoff_budget(),
                "deadline = 2 attempts + backoff budget"
            );
            // The incident shape: attempt 0 timed out (elapsed = timeout).
            // With the backoff budget in the deadline, the retry decision
            // admits the backoff and the second attempt genuinely runs.
            let decision = retry_wait_decision(true, true, 0, timeout, deadline);
            assert_eq!(decision, Some(1000), "first timeout retry waits 1s");
        }
    }

    // -----------------------------------------------------------------------
    // Body-read failure retry (connection dropped mid-body after HTTP 200)
    // -----------------------------------------------------------------------
    mod body_read_retry {
        use crate::llm::{next_body_read_retry_wait, MAX_CONNECTION_RETRIES, MAX_TIMEOUT_RETRIES};
        use std::time::Duration;

        const DEADLINE: Duration = Duration::from_secs(240);

        #[test]
        fn body_read_blip_retries_up_to_connection_cap() {
            for attempt in 0..MAX_CONNECTION_RETRIES {
                assert!(
                    next_body_read_retry_wait(false, attempt, Duration::ZERO, DEADLINE).is_some(),
                    "attempt {attempt} should retry"
                );
            }
            assert_eq!(
                next_body_read_retry_wait(false, MAX_CONNECTION_RETRIES, Duration::ZERO, DEADLINE),
                None
            );
        }

        #[test]
        fn body_read_timeout_retries_at_most_once() {
            assert!(next_body_read_retry_wait(true, 0, Duration::ZERO, DEADLINE).is_some());
            assert_eq!(
                next_body_read_retry_wait(true, MAX_TIMEOUT_RETRIES, Duration::ZERO, DEADLINE),
                None
            );
        }

        #[test]
        fn body_read_retry_respects_deadline() {
            assert_eq!(
                next_body_read_retry_wait(false, 0, Duration::from_secs(241), DEADLINE),
                None
            );
        }
    }

    // -----------------------------------------------------------------------
    // Transient server-error (HTTP 5xx body) retry
    // -----------------------------------------------------------------------
    mod server_error_retry {
        use crate::llm::{
            is_transient_server_error, next_server_error_retry_wait,
            server_error_retry_backoff_ms, MAX_5XX_RETRIES,
        };
        use std::time::Duration;

        const DEADLINE: Duration = Duration::from_secs(240);

        #[test]
        fn statuses_outside_5xx_range_are_not_transient() {
            assert!(!is_transient_server_error(400, "internal server error"));
            assert!(!is_transient_server_error(401, "unauthorized"));
            assert!(!is_transient_server_error(429, "rate limited"));
            assert!(!is_transient_server_error(529, "overloaded"));
            assert!(!is_transient_server_error(599, "server error"));
        }

        #[test]
        fn allowed_5xx_statuses_are_transient_when_body_empty() {
            for status in [500, 502, 503, 504] {
                assert!(
                    is_transient_server_error(status, ""),
                    "status {status} with empty body should be transient"
                );
            }
        }

        #[test]
        fn whitespace_only_body_treated_as_empty() {
            // Some providers return whitespace/newline-only 5xx bodies; these
            // should be treated the same as a truly empty (transient) body.
            for body in [" ", "\n", "  \n\t ", "\r\n"] {
                for status in [500, 502, 503, 504] {
                    assert!(
                        is_transient_server_error(status, body),
                        "status {status} body {:?} should be transient",
                        body
                    );
                }
            }
        }

        #[test]
        fn allowed_5xx_statuses_are_transient_for_known_phrases() {
            let bodies = [
                "overloaded",
                "Temporarily Unavailable",
                "Internal Server Error",
                "Bad Gateway",
                "Service Unavailable",
                "Gateway Timeout",
                "peg-native",
                "server_error",
                "try again",
                "Please try again later",
            ];
            for body in bodies {
                for status in [500, 502, 503, 504] {
                    assert!(
                        is_transient_server_error(status, body),
                        "status {status} body '{body}' should be transient"
                    );
                }
            }
        }

        #[test]
        fn non_matching_5xx_body_is_not_transient() {
            assert!(!is_transient_server_error(500, "{\"error\": \"invalid_request\"}"));
            assert!(!is_transient_server_error(500, "validation failed"));
            assert!(!is_transient_server_error(500, "permission denied"));
        }

        #[test]
        fn backoff_increases_linearly() {
            assert_eq!(server_error_retry_backoff_ms(0), 1500);
            assert_eq!(server_error_retry_backoff_ms(1), 3000);
            assert_eq!(server_error_retry_backoff_ms(2), 4500);
        }

        #[test]
        fn retries_up_to_max_5xx_retries() {
            assert_eq!(MAX_5XX_RETRIES, 2);
            for attempt in 0..MAX_5XX_RETRIES {
                assert!(
                    next_server_error_retry_wait(
                        500,
                        "internal server error",
                        attempt,
                        Duration::ZERO,
                        DEADLINE,
                    )
                    .is_some(),
                    "attempt {attempt} should retry"
                );
            }
            assert_eq!(
                next_server_error_retry_wait(
                    500,
                    "internal server error",
                    MAX_5XX_RETRIES,
                    Duration::ZERO,
                    DEADLINE,
                ),
                None
            );
        }

        #[test]
        fn non_transient_5xx_never_retries() {
            assert_eq!(
                next_server_error_retry_wait(
                    500,
                    "{\"error\": \"invalid_request\"}",
                    0,
                    Duration::ZERO,
                    DEADLINE,
                ),
                None
            );
        }

        #[test]
        fn deadline_stops_5xx_retries() {
            assert_eq!(
                next_server_error_retry_wait(
                    500,
                    "internal server error",
                    0,
                    Duration::from_secs(241),
                    DEADLINE,
                ),
                None
            );
        }

        #[test]
        fn backoff_crossing_deadline_stops_now() {
            // attempt 1 backoff is 3000ms; at 238s elapsed it would cross 240s.
            assert_eq!(
                next_server_error_retry_wait(
                    500,
                    "internal server error",
                    1,
                    Duration::from_millis(238_500),
                    DEADLINE,
                ),
                None
            );
        }
    }

    // ---- RFC #779 Part E.2: failover eligibility tests ----

    #[test]
    fn failover_eligible_on_rate_limit() {
        let err = anyhow::anyhow!("HTTP 429: too many requests");
        assert!(crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_eligible_on_5xx() {
        let err = anyhow::anyhow!("HTTP 503: Service Unavailable");
        assert!(crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_eligible_on_anthropic_overloaded() {
        let err = anyhow::anyhow!("{}", r#"HTTP 529: {"error":{"type":"overloaded_error"}}"#);
        assert!(crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_eligible_on_connection_refused() {
        let err = anyhow::anyhow!("error sending request: connection refused");
        assert!(crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_eligible_on_timeout() {
        let err = anyhow::anyhow!("request timed out after 120s");
        assert!(crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_not_eligible_on_bad_request() {
        let err = anyhow::anyhow!("HTTP 400: invalid model name");
        assert!(!crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_not_eligible_on_auth_error() {
        let err = anyhow::anyhow!("HTTP 401: invalid api key");
        assert!(!crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_not_eligible_on_forbidden() {
        let err = anyhow::anyhow!("HTTP 403: forbidden");
        assert!(!crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_not_eligible_on_generic_validation() {
        let err = anyhow::anyhow!("response validation failed: missing required field");
        assert!(!crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_not_eligible_on_context_overflow() {
        // Context overflow is handled by the context governor (P-6.9), not
        // by failover — even if the message contains a 5xx status.
        let err = anyhow::anyhow!(
            "context_length_exceeded: status=400 context_overflow"
        );
        assert!(!crate::llm::is_failover_eligible_error(&err));
    }

    #[test]
    fn failover_not_eligible_on_resource_exhausted() {
        let err = anyhow::anyhow!("RESOURCE_EXHAUSTED: context window exceeded");
        assert!(!crate::llm::is_failover_eligible_error(&err));
    }
}

// ---------------------------------------------------------------------------
// Stall-detecting streaming turn path (#1044)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod stall_detection {
    use crate::llm::{
        complete_with_stall_detection, CompletionRequest, CompletionResponse, LlmDriver,
        StopReason, StreamEvent, TokenUsage,
    };
    use std::sync::Arc;
    use std::time::Duration;

    /// A driver that emits scripted chunks with scripted delays, for timing
    /// tests without a network. `request_timeout` doubles as the idle-gap
    /// budget the stall detector enforces.
    struct ScriptedDriver {
        script: Vec<(Duration, Option<StreamEvent>)>,
        idle: Duration,
    }

    impl ScriptedDriver {
        fn ok_response() -> CompletionResponse {
            CompletionResponse {
                text: "hello".to_string(),
                tool_calls: vec![],
                reasoning_content: None,
                reasoning_details: None,
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmDriver for ScriptedDriver {
        async fn complete(
            &self,
            _req: &CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            Ok(Self::ok_response())
        }

        async fn stream(
            &self,
            _req: &CompletionRequest,
            tx: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> anyhow::Result<CompletionResponse> {
            for (delay, event) in &self.script {
                tokio::time::sleep(*delay).await;
                if let Some(ev) = event {
                    let _ = tx.send(ev.clone()).await;
                }
            }
            Ok(Self::ok_response())
        }

        fn request_timeout(&self) -> Duration {
            self.idle
        }
    }

    fn req() -> CompletionRequest {
        CompletionRequest {
            model: "test".to_string(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            metadata: None,
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        }
    }

    /// The incident shape: zero bytes in the whole budget. The error must
    /// name the stall phase and carry the retryable `llm_transport:timeout`
    /// token so the workflow layer retries mechanically (#1041).
    #[tokio::test]
    async fn stall_before_first_byte_is_a_retryable_timeout() {
        let driver: Arc<dyn LlmDriver> = Arc::new(ScriptedDriver {
            script: vec![(Duration::from_millis(500), None)], // silence past the gap
            idle: Duration::from_millis(50),
        });
        let err = complete_with_stall_detection(&driver, &req())
            .await
            .expect_err("stall must error");
        let msg = err.to_string();
        assert!(msg.contains("llm_transport:timeout"), "{msg}");
        assert!(msg.contains("stalled before first byte"), "{msg}");
    }

    /// A stall after partial output is mid-stream; the phase distinguishes
    /// "upstream stalled" from "slow generation" in the log.
    #[tokio::test]
    async fn stall_mid_stream_reports_phase() {
        let driver: Arc<dyn LlmDriver> = Arc::new(ScriptedDriver {
            script: vec![
                (Duration::from_millis(10), Some(StreamEvent::TextDelta("hi".into()))),
                (Duration::from_millis(500), None), // then silence
            ],
            idle: Duration::from_millis(50),
        });
        let err = complete_with_stall_detection(&driver, &req())
            .await
            .expect_err("stall must error");
        let msg = err.to_string();
        assert!(msg.contains("llm_transport:timeout"), "{msg}");
        assert!(msg.contains("stalled mid-stream"), "{msg}");
    }

    /// A stream that keeps emitting within the gap budget completes even when
    /// its total duration far exceeds the old wall-clock cap — the whole point
    /// of gap-based budgeting.
    #[tokio::test]
    async fn long_generation_under_gap_budget_completes() {
        let driver: Arc<dyn LlmDriver> = Arc::new(ScriptedDriver {
            script: (0..10)
                .map(|i| {
                    (
                        Duration::from_millis(30), // each gap under the 50ms budget
                        Some(StreamEvent::TextDelta(format!("chunk{i}"))),
                    )
                })
                .collect(), // total ~300ms, 6× the 50ms idle budget
            idle: Duration::from_millis(50),
        });
        let resp = complete_with_stall_detection(&driver, &req())
            .await
            .expect("long but chatty stream must complete");
        assert_eq!(resp.text, "hello");
    }

    /// A driver error propagates through the detector unchanged (it already
    /// carries its own llm_transport token from the driver layer).
    #[tokio::test]
    async fn driver_error_propagates() {
        struct FailDriver;
        #[async_trait::async_trait]
        impl LlmDriver for FailDriver {
            async fn complete(
                &self,
                _req: &CompletionRequest,
            ) -> anyhow::Result<CompletionResponse> {
                anyhow::bail!("llm_transport:connect attempts=1 elapsed_ms=3 source_chain=[]: refused")
            }
            fn request_timeout(&self) -> Duration {
                Duration::from_millis(50)
            }
        }
        let driver: Arc<dyn LlmDriver> = Arc::new(FailDriver);
        let err = complete_with_stall_detection(&driver, &req())
            .await
            .expect_err("driver error must propagate");
        assert!(err.to_string().contains("llm_transport:connect"));
    }

    /// Review (#1080): a driver that sends `Complete` but keeps the sender
    /// alive (a cloned tx held by trailing cleanup) must not trip a false
    /// stall after a finished turn — detection ends on the Complete event,
    /// not on sender drop.
    #[tokio::test]
    async fn complete_event_ends_detection_even_if_sender_stays_alive() {
        struct LingeringSenderDriver;
        #[async_trait::async_trait]
        impl LlmDriver for LingeringSenderDriver {
            async fn complete(
                &self,
                _req: &CompletionRequest,
            ) -> anyhow::Result<CompletionResponse> {
                Ok(ScriptedDriver::ok_response())
            }
            async fn stream(
                &self,
                _req: &CompletionRequest,
                tx: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> anyhow::Result<CompletionResponse> {
                let lingering = tx.clone();
                let _ = tx.send(StreamEvent::TextDelta("hi".into())).await;
                let _ = tx
                    .send(StreamEvent::Complete {
                        stop_reason: StopReason::EndTurn,
                        usage: TokenUsage::default(),
                    })
                    .await;
                // A cleanup task holds a sender clone well past the idle
                // budget — with drop-based detection this would be a false
                // stall; with Complete-based detection it completes.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    drop(lingering);
                });
                Ok(ScriptedDriver::ok_response())
            }
            fn request_timeout(&self) -> Duration {
                Duration::from_millis(50)
            }
        }
        let driver: Arc<dyn LlmDriver> = Arc::new(LingeringSenderDriver);
        let resp = complete_with_stall_detection(&driver, &req())
            .await
            .expect("a finished turn must not false-stall on a lingering sender");
        assert_eq!(resp.text, "hello");
    }
}
