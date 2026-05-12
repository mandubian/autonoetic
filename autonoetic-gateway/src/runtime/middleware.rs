//! Middleware Pipeline for Agent Execution.
//!
//! Contains schema validation helpers and middleware hook execution
//! (pre-process and post-process) that run around LLM calls in the
//! agent execution loop.

use crate::runtime::lifecycle::AgentExecutor;
use crate::runtime::session_tracer::SessionTracer;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub(crate) struct SchemaValidation {
    pub(crate) valid: bool,
    pub(crate) messages: Vec<String>,
}

impl AgentExecutor {
    pub(crate) fn log_output_schema_validation(
        &self,
        response: &crate::llm::CompletionResponse,
        tracer: &mut SessionTracer,
    ) {
        // Only validate final output when agent claims completion (EndTurn).
        // Skip validation for tool use responses - agents may emit free text
        // alongside tool calls, which is expected reasoning/narration.
        if !matches!(
            response.stop_reason,
            crate::llm::StopReason::EndTurn | crate::llm::StopReason::StopSequence
        ) {
            return;
        }

        let Some(returns_schema) = self.manifest.io.as_ref().and_then(|io| io.returns.as_ref())
        else {
            return;
        };

        let validation = validate_against_schema(&response.text, returns_schema);
        let _ = tracer.log_event(
            "agent.process",
            "output_schema_validation",
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(serde_json::json!({
                "valid": validation.valid,
                "messages": validation.messages,
            })),
        );
    }

    /// Executes middleware pre-process script in a sandbox.
    pub(crate) fn apply_middleware_pre(
        &self,
        mut req: crate::llm::CompletionRequest,
        hook_script: &str,
        active_agent_dir: &Path,
        session_id: &str,
        turn_id: &str,
        tracer: &mut SessionTracer,
    ) -> anyhow::Result<crate::llm::CompletionRequest> {
        let _ = tracer.log_event(
            "agent.process",
            "pre_hook_requested",
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(serde_json::json!({ "turn_id": turn_id })),
        );

        let input_json = serde_json::to_string(&req)?;
        let output =
            self.run_middleware_script(hook_script, input_json, active_agent_dir, session_id)?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(transformed) = serde_json::from_str::<serde_json::Value>(&stdout) {
                if let Ok(new_req) =
                    serde_json::from_value::<crate::llm::CompletionRequest>(transformed.clone())
                {
                    req = new_req;
                } else if let Some(skip) = transformed.get("skip_llm").and_then(|v| v.as_bool()) {
                    let mut meta = req.metadata.unwrap_or_default();
                    meta.insert("skip_llm".to_string(), serde_json::Value::Bool(skip));
                    if let Some(reply) = transformed.get("assistant_reply").and_then(|v| v.as_str())
                    {
                        meta.insert(
                            "assistant_reply".to_string(),
                            serde_json::Value::String(reply.to_string()),
                        );
                    }
                    req.metadata = Some(meta);
                }
                let _ = tracer.log_event(
                    "agent.process",
                    "pre_hook_completed",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    None,
                );
                Ok(req)
            } else {
                let _ = tracer.log_event(
                    "agent.process",
                    "pre_hook_failed",
                    autonoetic_types::causal_chain::EntryStatus::Error,
                    Some(serde_json::json!({ "error": "Invalid JSON from hook" })),
                );
                anyhow::bail!("Pre-process hook returned invalid JSON");
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = tracer.log_event(
                "agent.process",
                "pre_hook_failed",
                autonoetic_types::causal_chain::EntryStatus::Error,
                Some(serde_json::json!({ "error": stderr })),
            );
            anyhow::bail!("Pre-process hook failed: {}", stderr);
        }
    }

    /// Executes middleware post-process script in a sandbox.
    pub(crate) fn apply_middleware_post(
        &self,
        mut response: crate::llm::CompletionResponse,
        hook_script: &str,
        active_agent_dir: &Path,
        session_id: &str,
        turn_id: &str,
        tracer: &mut SessionTracer,
    ) -> anyhow::Result<crate::llm::CompletionResponse> {
        let _ = tracer.log_event(
            "agent.process",
            "post_hook_requested",
            autonoetic_types::causal_chain::EntryStatus::Success,
            Some(serde_json::json!({ "turn_id": turn_id })),
        );

        let input_json = serde_json::to_string(&response)?;
        let output =
            self.run_middleware_script(hook_script, input_json, active_agent_dir, session_id)?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(transformed) = serde_json::from_str::<crate::llm::CompletionResponse>(&stdout)
            {
                response = transformed;
                let _ = tracer.log_event(
                    "agent.process",
                    "post_hook_completed",
                    autonoetic_types::causal_chain::EntryStatus::Success,
                    None,
                );
                Ok(response)
            } else {
                let _ = tracer.log_event(
                    "agent.process",
                    "post_hook_failed",
                    autonoetic_types::causal_chain::EntryStatus::Error,
                    Some(serde_json::json!({ "error": "Invalid JSON from hook" })),
                );
                anyhow::bail!("Post-process hook returned invalid JSON");
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = tracer.log_event(
                "agent.process",
                "post_hook_failed",
                autonoetic_types::causal_chain::EntryStatus::Error,
                Some(serde_json::json!({ "error": stderr })),
            );
            anyhow::bail!("Post-process hook failed: {}", stderr);
        }
    }

    fn run_middleware_script(
        &self,
        command: &str,
        stdin_json: String,
        active_agent_dir: &Path,
        _session_id: &str,
    ) -> anyhow::Result<std::process::Output> {
        use crate::sandbox::{SandboxDriverKind, SandboxRunner};
        use std::io::Write;

        let driver = SandboxDriverKind::parse(&self.manifest.runtime.sandbox)?;
        let agent_dir_str = active_agent_dir
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid active_agent_dir"))?;

        let mut runner = SandboxRunner::spawn_with_driver_and_dependencies(
            driver,
            agent_dir_str,
            command,
            None,
            None,
        )?;

        if let Some(mut stdin) = runner.process.stdin.take() {
            stdin.write_all(stdin_json.as_bytes())?;
        }

        runner.process.wait_with_output().map_err(Into::into)
    }
}

/// Extracts JSON from markdown-wrapped content.
/// Handles common LLM output formats:
/// - ```json ... ``` (code block with json language hint)
/// - ``` ... ``` (plain code block)
/// - Plain JSON without markdown wrapping
pub(crate) fn extract_json_from_markdown(input: &str) -> String {
    let trimmed = input.trim();

    // Try to find ```json ... ``` or ``` ... ``` blocks
    if let Some(start) = trimmed.find("```") {
        let after_first_block = &trimmed[start + 3..];

        // Skip language hint (e.g., "json\n" -> "\n")
        let content_start = after_first_block.find('\n').map(|i| i + 1).unwrap_or(0);
        let content = &after_first_block[content_start..];

        // Find closing ```
        if let Some(end) = content.find("```") {
            return content[..end].trim().to_string();
        }
    }

    // No markdown wrapping found, return original
    input.to_string()
}

/// Lightweight schema validation: checks required fields and basic type hints.
/// Extracts JSON from markdown-wrapped content before validation.
pub(crate) fn validate_against_schema(input: &str, schema: &serde_json::Value) -> SchemaValidation {
    let mut validation = SchemaValidation {
        valid: true,
        messages: Vec::new(),
    };

    // Extract JSON from markdown if present
    let json_input = extract_json_from_markdown(input);

    let parsed_input: serde_json::Value = match serde_json::from_str(&json_input) {
        Ok(v) => v,
        Err(_) => {
            validation.valid = false;
            validation
                .messages
                .push("Output is not valid JSON".to_string());
            return validation;
        }
    };

    if let Some(expected_type) = schema.get("type").and_then(|t| t.as_str()) {
        let type_matches = match expected_type {
            "object" => parsed_input.is_object(),
            "array" => parsed_input.is_array(),
            "string" => parsed_input.is_string(),
            "number" => parsed_input.is_number(),
            "boolean" => parsed_input.is_boolean(),
            _ => true,
        };
        if !type_matches {
            validation.valid = false;
            validation.messages.push(format!(
                "Type mismatch: expected {}, got {}",
                expected_type, parsed_input
            ));
        }
    }

    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        if let Some(obj) = parsed_input.as_object() {
            for field in required {
                if let Some(field_name) = field.as_str() {
                    if !obj.contains_key(field_name) {
                        validation.valid = false;
                        validation
                            .messages
                            .push(format!("Missing required field: {}", field_name));
                    }
                }
            }
        }
    }

    validation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CompletionResponse, LlmDriver, StopReason, TokenUsage, ToolCall};
    use crate::runtime::tools::default_registry;
    use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
    use autonoetic_types::capability::Capability;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn manifest_with_capabilities(capabilities: Vec<Capability>) -> AgentManifest {
        AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "test-agent".to_string(),
                name: "test-agent".to_string(),
                description: "test".to_string(),
            },
            capabilities,
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        }
    }

    struct FixedTextDriver;
    #[async_trait::async_trait]
    impl LlmDriver for FixedTextDriver {
        async fn complete(
            &self,
            _request: &crate::llm::CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: "assistant reply".to_string(),
                tool_calls: vec![],
                reasoning_content: None,
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
    }

    #[test]
    fn test_extract_json_from_markdown_plain_json() {
        let input = r#"{"findings":["fact1"],"summary":"ok"}"#;
        let extracted = extract_json_from_markdown(input);
        assert_eq!(extracted, input);
    }

    #[test]
    fn test_extract_json_from_markdown_json_code_block() {
        let input = r#"Here is the result:
```json
{"findings":["fact1"],"summary":"ok"}
```
Hope this helps!"#;
        let extracted = extract_json_from_markdown(input);
        let expected = r#"{"findings":["fact1"],"summary":"ok"}"#;
        assert_eq!(extracted, expected);
    }

    #[test]
    fn test_extract_json_from_markdown_plain_code_block() {
        let input = r#"Result:
```
{"findings":["fact1"],"summary":"ok"}
```"#;
        let extracted = extract_json_from_markdown(input);
        let expected = r#"{"findings":["fact1"],"summary":"ok"}"#;
        assert_eq!(extracted, expected);
    }

    #[test]
    fn test_extract_json_from_markdown_multiline_json() {
        let input = r#"```json
{
  "findings": ["fact1", "fact2"],
  "summary": "ok"
}
```"#;
        let extracted = extract_json_from_markdown(input);
        let expected = r#"{
  "findings": ["fact1", "fact2"],
  "summary": "ok"
}"#;
        assert_eq!(extracted, expected);
    }

    #[test]
    fn test_validate_output_schema_valid_json_input() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["findings", "summary"]
        });
        let output = r#"{"findings":["fact1"],"summary":"ok"}"#;
        let result = validate_against_schema(output, &schema);
        assert!(result.valid);
        assert!(result.messages.is_empty());
    }

    #[test]
    fn test_validate_output_schema_non_json_input() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["findings"]
        });
        let output = "plain text response";
        let result = validate_against_schema(output, &schema);
        assert!(!result.valid);
        assert!(result.messages.iter().any(|m| m.contains("not valid JSON")));
    }

    #[test]
    fn test_validate_output_schema_accepts_markdown_wrapped_json() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["findings", "summary"]
        });
        let output = r#"Here is the result:
```json
{"findings":["fact1"],"summary":"ok"}
```
Hope this helps!"#;
        let result = validate_against_schema(output, &schema);
        assert!(
            result.valid,
            "Should accept markdown-wrapped JSON: {:?}",
            result.messages
        );
    }

    #[test]
    fn test_log_output_schema_validation_skips_tool_use() {
        let manifest = manifest_with_capabilities(vec![]);
        let temp = tempdir().expect("tempdir should create");
        let executor = AgentExecutor::new(
            manifest,
            "p".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            default_registry(),
            None,
        );

        let mut tracer = crate::runtime::session_tracer::SessionTracer::test_tracer();

        // ToolUse with any text should be skipped - no validation
        let response = CompletionResponse {
            text: "Let me check the database first...".to_string(),
            tool_calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "any".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        };

        executor.log_output_schema_validation(&response, &mut tracer);
    }

    #[test]
    fn test_log_output_schema_validation_validates_end_turn() {
        let mut manifest = manifest_with_capabilities(vec![]);
        manifest.io = Some(autonoetic_types::agent::AgentIO {
            accepts: None,
            returns: Some(serde_json::json!({
                "type": "object",
                "required": ["result"]
            })),
            output_policy: None,
        });

        let temp = tempdir().expect("tempdir should create");
        let executor = AgentExecutor::new(
            manifest,
            "p".to_string(),
            Arc::new(FixedTextDriver),
            temp.path().to_path_buf(),
            default_registry(),
            None,
        );

        let mut tracer = crate::runtime::session_tracer::SessionTracer::test_tracer();

        // EndTurn with invalid JSON should produce validation error
        let response = CompletionResponse {
            text: "plain text response".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };

        executor.log_output_schema_validation(&response, &mut tracer);

        // EndTurn with valid JSON matching schema should pass
        let mut tracer2 = crate::runtime::session_tracer::SessionTracer::test_tracer();
        let response2 = CompletionResponse {
            text: r#"{"result": "success"}"#.to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };

        executor.log_output_schema_validation(&response2, &mut tracer2);
    }
}
