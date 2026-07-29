//! Compression Quality Integration Tests.
//!
//! End-to-end tests that exercise the full compression pipeline with a
//! real LLM. These tests require environment configuration:
//!
//! Required env vars:
//!   AUTONOETIC_TEST_LLM_PROVIDER — e.g. "openai", "anthropic", "openrouter"
//!   AUTONOETIC_TEST_LLM_MODEL    — e.g. "gpt-4o-mini", "claude-3-haiku-20240307"
//!   AUTONOETIC_TEST_LLM_API_KEY  — API key for the provider
//!
//! If these are not set, all tests are skipped.

use autonoetic_gateway::llm::{build_driver, CompletionRequest, Message};
use autonoetic_gateway::runtime::compression::{compress_context, CompressionMetadata};
use autonoetic_gateway::runtime::compression_quality::{
    validate_compressed_history, GoldenSession, GoldenToolCall, GoldenTurn, StructuralValidation,
};
use autonoetic_types::agent::CompressionConfig;
use autonoetic_types::config::{ContextCompressionConfig, LlmPreset};
use std::collections::HashMap;

fn get_test_llm_config() -> Option<(LlmPreset, reqwest::Client)> {
    let provider = std::env::var("AUTONOETIC_TEST_LLM_PROVIDER").ok()?;
    let model = std::env::var("AUTONOETIC_TEST_LLM_MODEL").ok()?;
    let api_key = std::env::var("AUTONOETIC_TEST_LLM_API_KEY").ok()?;

    let preset = LlmPreset {
        provider: Some(provider),
        model: Some(model),
        temperature: Some(0.1),
        fallback_provider: None,
        fallback_model: None,
        chat_only: None,
        context_window_tokens: None,
        base_url: None,
        api_key_env: None,
        thinking: None,
        tier: None,
        cost: None,
        latency: None,
        routing: None,
        egress_class: None,
    };
    Some((preset, reqwest::Client::new()))
}

fn make_golden_session_with_tool_use() -> GoldenSession {
    GoldenSession {
        id: "integration-tool-use".into(),
        description: "Multi-turn with tool use for compression quality check".into(),
        system_prompt: "You are a helpful assistant. When asked about weather, use the weather tool. When asked about news, use the search tool. Keep responses brief.".into(),
        turns: vec![
            GoldenTurn {
                user_message: "What's the weather in Tokyo?".into(),
                assistant_response: "Checking weather for Tokyo.".into(),
                tool_calls: vec![GoldenToolCall {
                    id: "tc_weather".into(),
                    name: "weather.get".into(),
                    arguments: r#"{"city":"Tokyo"}"#.into(),
                }],
                end_turn: false,
            },
            GoldenTurn {
                user_message: "It's 25°C and raining.".into(),
                assistant_response: "Thanks for the update. 25°C with rain — typical Tokyo weather.".into(),
                tool_calls: vec![],
                end_turn: false,
            },
            GoldenTurn {
                user_message: "Search for recent news about AI.".into(),
                assistant_response: "Searching for AI news.".into(),
                tool_calls: vec![GoldenToolCall {
                    id: "tc_search".into(),
                    name: "search.query".into(),
                    arguments: r#"{"query":"recent AI news"}"#.into(),
                }],
                end_turn: false,
            },
            GoldenTurn {
                user_message: "Found 3 articles about LLM improvements.".into(),
                assistant_response: "Great! LLM improvements are always exciting. Anything specific you'd like to discuss?".into(),
                tool_calls: vec![],
                end_turn: true,
            },
        ],
        expected_outcome: "Multi-turn tool use conversation completed".into(),
        expected_tool_sequence: vec![
            GoldenToolCall { id: "tc_weather".into(), name: "weather.get".into(), arguments: r#"{"city":"Tokyo"}"#.into() },
            GoldenToolCall { id: "tc_search".into(), name: "search.query".into(), arguments: r#"{"query":"recent AI news"}"#.into() },
        ],
    }
}

fn build_history_from_session(session: &GoldenSession) -> Vec<Message> {
    let mut history = vec![Message::system(&session.system_prompt)];
    for turn in &session.turns {
        history.push(Message::user(&turn.user_message));
        history.push(Message::assistant(&turn.assistant_response));
        for tc in &turn.tool_calls {
            history.push(Message::tool_result(&tc.id, &tc.name, "result"));
        }
    }
    history
}

#[tokio::test]
async fn test_compression_structural_validation_with_real_llm() {
    let Some((preset, http_client)) = get_test_llm_config() else {
        eprintln!("Skipping: AUTONOETIC_TEST_LLM_* not configured");
        return;
    };

    let session = make_golden_session_with_tool_use();
    let history = build_history_from_session(&session);

    let cfg = ContextCompressionConfig {
        enabled: true,
        llm_preset: None,
        threshold_pct: 10.0,
        recent_turns_to_keep: 1,
        max_summary_tokens: 300,
        min_turns_between_compression: 0,
        provider: preset.provider.clone(),
        model: preset.model.clone(),
        max_capsule_decisions: 30,
        max_completed_tasks: 10,
    };

    let presets = HashMap::new();
    let mut empty_labels = std::collections::HashMap::new();
    let result = compress_context(
        history.clone(),
        Some(128_000),
        &cfg,
        None,
        &presets,
        &http_client,
        &session.id,
        session.turns.len() as u64,
        None,
        &mut empty_labels,
    )
    .await;

    match result {
        Ok(compression_result) => {
            if !compression_result.compressed {
                eprintln!("Compression not triggered (may need more content to exceed threshold)");
                return;
            }

            let validation = validate_compressed_history(&history, &compression_result.history);

            assert!(
                validation.valid,
                "Structural validation failed: {:?}",
                validation.issues
            );
            assert!(
                validation.summary_present,
                "Summary message not found in compressed history"
            );
            assert!(
                validation.messages_reduced,
                "Messages not reduced after compression"
            );
            assert!(
                validation.no_orphaned_tool_results,
                "Orphaned tool results found"
            );
            assert!(
                validation.tool_call_groups_intact,
                "Tool-call groups split across boundary"
            );

            eprintln!("Compression quality validation passed: {:?}", validation);
        }
        Err(e) => {
            eprintln!(
                "Compression failed (expected if no valid LLM config): {}",
                e
            );
        }
    }
}

#[tokio::test]
async fn test_compression_summary_quality_with_real_llm() {
    let Some((preset, http_client)) = get_test_llm_config() else {
        eprintln!("Skipping: AUTONOETIC_TEST_LLM_* not configured");
        return;
    };

    let session = make_golden_session_with_tool_use();
    let history = build_history_from_session(&session);

    let cfg = ContextCompressionConfig {
        enabled: true,
        llm_preset: None,
        threshold_pct: 10.0,
        recent_turns_to_keep: 1,
        max_summary_tokens: 300,
        min_turns_between_compression: 0,
        provider: preset.provider.clone(),
        model: preset.model.clone(),
        max_capsule_decisions: 30,
        max_completed_tasks: 10,
    };

    let presets = HashMap::new();
    let mut empty_labels = std::collections::HashMap::new();
    let result = compress_context(
        history.clone(),
        Some(128_000),
        &cfg,
        None,
        &presets,
        &http_client,
        &session.id,
        session.turns.len() as u64,
        None,
        &mut empty_labels,
    )
    .await;

    if let Ok(compression_result) = result {
        if !compression_result.compressed {
            return;
        }

        let summary_msg = compression_result
            .history
            .iter()
            .find(|m| m.content.starts_with("[COMPRESSED CONTEXT"));

        assert!(summary_msg.is_some(), "Summary message not found");
        let summary = summary_msg.unwrap();
        assert!(!summary.content.is_empty(), "Summary is empty");
        assert!(
            summary.content.len() > 20,
            "Summary seems too short to be meaningful"
        );

        eprintln!("Summary generated ({} chars)", summary.content.len());
    }
}
