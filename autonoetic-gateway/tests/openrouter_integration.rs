//! Integration test — makes a live HTTP call to OpenRouter.
//!
//! Run with:
//!   OPENROUTER_API_KEY=<key> cargo test -p autonoetic-gateway --test openrouter_integration -- --nocapture
//!
//! Skipped automatically when OPENROUTER_API_KEY is not set.

use autonoetic_gateway::llm::{self, CompletionRequest, LlmDriver, Message};

/// Resolve a provider and build the driver the same way the gateway would.
fn make_openrouter_driver(model: &str) -> anyhow::Result<std::sync::Arc<dyn LlmDriver>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let resolved = llm::provider::resolve(
        "openrouter",
        model,
        Some(0.7),
        Some(512),
        None,
        None,  // reads OPENROUTER_API_KEY from env
        false, // chat_only
    )?;

    use autonoetic_gateway::llm::openai::OpenAiDriver;
    use std::sync::Arc;
    Ok(Arc::new(OpenAiDriver::new(client, resolved)))
}

#[tokio::test]
async fn test_openrouter_simple_completion() -> anyhow::Result<()> {
    // Skip if key not set
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("Skipping integration test: OPENROUTER_API_KEY not set");
        return Ok(());
    }

    // Model from agent_config.toml → balanced_gfl
    let model = "google/gemini-3-flash-preview";
    let driver = make_openrouter_driver(model)?;

    let req = CompletionRequest::simple(
        model,
        vec![
            Message::system("You are a concise assistant. Answer in one sentence."),
            Message::user("What is 2 + 2? Reply with only the number and one word."),
        ],
    );

    let resp = driver.complete(&req).await?;

    println!("=== OpenRouter response ===");
    println!("Text:        {:?}", resp.text);
    println!("Stop reason: {:?}", resp.stop_reason);
    println!(
        "Usage:       in={} out={}",
        resp.usage.input_tokens, resp.usage.output_tokens
    );

    assert!(!resp.text.is_empty(), "Expected non-empty response");
    assert!(
        resp.text.contains('4'),
        "Expected answer to contain '4', got: {}",
        resp.text
    );

    Ok(())
}

#[tokio::test]
async fn test_openrouter_tool_call() -> anyhow::Result<()> {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("Skipping integration test: OPENROUTER_API_KEY not set");
        return Ok(());
    }

    let model = "google/gemini-3-flash-preview";
    let driver = make_openrouter_driver(model)?;

    use autonoetic_gateway::llm::ToolDefinition;
    use serde_json::json;

    let req = CompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message::system(
                "You are a helpful assistant. Use the provided tools when appropriate.",
            ),
            Message::user("What is the current weather in Paris? Use the get_weather tool."),
        ],
        tools: vec![ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get the current weather for a given city.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string", "description": "City name" }
                },
                "required": ["city"]
            }),
        }],
        max_tokens: Some(512),
        temperature: Some(0.0),
        metadata: None,
    };

    let resp = driver.complete(&req).await?;

    println!("=== OpenRouter tool call response ===");
    println!("Text:        {:?}", resp.text);
    println!("Tool calls:  {:?}", resp.tool_calls);
    println!("Stop reason: {:?}", resp.stop_reason);

    // Model should call the tool
    assert!(
        !resp.tool_calls.is_empty(),
        "Expected at least one tool call"
    );
    assert_eq!(resp.tool_calls[0].name, "get_weather");

    let args: serde_json::Value = serde_json::from_str(&resp.tool_calls[0].arguments)?;
    let city = args["city"].as_str().unwrap_or("");
    println!("Tool called with city: {}", city);
    assert!(
        city.to_lowercase().contains("paris"),
        "Expected city=Paris, got: {}",
        city
    );

    Ok(())
}

/// Integration test — validates that tool compression (replacing full schemas
/// with `{}`) still produces valid tool calls on subsequent turns.
///
/// Run with:
///   OPENROUTER_API_KEY=<key> cargo test -p autonoetic-gateway --test openrouter_integration -- --nocapture tool_compression
///
/// Skipped automatically when OPENROUTER_API_KEY is not set.
#[tokio::test]
async fn test_openrouter_tool_compression() -> anyhow::Result<()> {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("Skipping integration test: OPENROUTER_API_KEY not set");
        return Ok(());
    }

    let model = "google/gemini-3-flash-preview";
    let driver = make_openrouter_driver(model)?;

    use autonoetic_gateway::llm::ToolDefinition;
    use serde_json::json;

    // Turn 0: full tool schema
    let full_tool = ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the current weather for a given city.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "city": { "type": "string", "description": "City name" },
                "unit": { "type": "string", "enum": ["celsius", "fahrenheit"], "description": "Temperature unit" }
            },
            "required": ["city"]
        }),
    };

    let req_turn0 = CompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message::system("You are a helpful assistant. Use tools when appropriate."),
            Message::user("What's the weather in Tokyo?"),
        ],
        tools: vec![full_tool],
        max_tokens: Some(512),
        temperature: Some(0.0),
        metadata: None,
    };

    let resp_turn0 = driver.complete(&req_turn0).await?;
    println!("=== Turn 0 (full schema) ===");
    println!("Tool calls: {:?}", resp_turn0.tool_calls);
    assert!(!resp_turn0.tool_calls.is_empty(), "Turn 0: expected tool call");
    assert_eq!(resp_turn0.tool_calls[0].name, "get_weather");

    // Turn 1: compressed tool schema (empty `{}`) — this is what compress_tool_definitions does
    let compressed_tool = ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the current weather for a given city.".to_string(),
        input_schema: json!({}),
    };

    let args_turn0: serde_json::Value = serde_json::from_str(&resp_turn0.tool_calls[0].arguments)?;
    let city_turn0 = args_turn0["city"].as_str().unwrap_or("Tokyo");

    let req_turn1 = CompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message::system("You are a helpful assistant. Use tools when appropriate."),
            Message::user("What's the weather in Tokyo?"),
            Message::tool_result(
                &resp_turn0.tool_calls[0].id,
                "get_weather",
                format!("Sunny, 22°C in {}", city_turn0),
            ),
            Message::assistant("The weather in Tokyo is sunny, 22°C."),
            Message::user("How about in Seoul?"),
        ],
        tools: vec![compressed_tool],
        max_tokens: Some(512),
        temperature: Some(0.0),
        metadata: None,
    };

    let resp_turn1 = driver.complete(&req_turn1).await?;
    println!("=== Turn 1 (compressed schema) ===");
    println!("Tool calls: {:?}", resp_turn1.tool_calls);

    // The model should still be able to call the tool with compressed schema
    assert!(
        !resp_turn1.tool_calls.is_empty(),
        "Turn 1 (compressed): expected tool call — model should still know the tool"
    );
    assert_eq!(
        resp_turn1.tool_calls[0].name, "get_weather",
        "Turn 1 (compressed): expected get_weather tool call"
    );

    let args_turn1: serde_json::Value = serde_json::from_str(&resp_turn1.tool_calls[0].arguments)?;
    let city_turn1 = args_turn1["city"].as_str().unwrap_or("");
    println!("Turn 1 tool called with city: {}", city_turn1);
    assert!(
        city_turn1.to_lowercase().contains("seoul"),
        "Turn 1 (compressed): expected city=Seoul, got: {}",
        city_turn1
    );

    println!("=== Tool compression validation PASSED ===");
    Ok(())
}
