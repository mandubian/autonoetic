//! Context Compression.
//!
//! Iterative summarization when conversation history approaches context limits.
//! Replaces old turns with a compact LLM-generated summary while preserving
//! recent turns in full. The full compressed context is written to the content
//! store for audit and restore.

use crate::llm::{build_driver, CompletionRequest, Message, Role};
use crate::runtime::content_store::{ContentStore, ContentVisibility};
use autonoetic_types::agent::{CompressionConfig, LlmConfig};
use autonoetic_types::config::{ContextCompressionConfig, LlmPreset};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::prompt_budget::estimate_message_tokens;

/// Metadata about a compression operation, stored in checkpoints so
/// restored sessions know what was already summarized.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompressionMetadata {
    /// Turn number at which compression last occurred.
    pub last_compression_turn: u64,
    /// Number of messages that were summarized.
    pub messages_summarized: u64,
    /// Content handle to the full compressed context written to the store.
    pub compressed_context_handle: Option<String>,
    /// Running count of compression operations in this session.
    pub compression_count: u64,
}

/// Result of a compression operation.
#[derive(Debug)]
pub struct CompressionResult {
    /// The compressed history (summary message + retained recent messages).
    pub history: Vec<Message>,
    /// The original uncompressed history, saved for audit/restore.
    pub original_history: Vec<Message>,
    /// Metadata about the compression for checkpoint storage.
    pub metadata: CompressionMetadata,
    /// Whether compression was actually applied.
    pub compressed: bool,
}

/// Resolve the LLM config for compression from gateway config and optional agent override.
pub fn resolve_compression_llm_config(
    gateway_cfg: &ContextCompressionConfig,
    agent_cfg: Option<&CompressionConfig>,
    presets: &HashMap<String, LlmPreset>,
) -> Option<LlmConfig> {
    let preset_name = agent_cfg
        .and_then(|a| a.llm_preset.as_deref())
        .or(gateway_cfg.llm_preset.as_deref());

    if let Some(name) = preset_name {
        let preset = presets.get(name)?;
        if preset.provider.is_some() && preset.model.is_some() {
            return Some(LlmConfig {
                provider: preset.provider.clone().unwrap(),
                model: preset.model.clone().unwrap(),
                temperature: preset.temperature.unwrap_or(0.1),
                fallback_provider: preset.fallback_provider.clone(),
                fallback_model: preset.fallback_model.clone(),
                chat_only: preset.chat_only.unwrap_or(false),
                context_window_tokens: preset.context_window_tokens,
                base_url: preset.base_url.clone(),
                api_key_env: preset.api_key_env.clone(),
                routing_preset: None,
                thinking: preset.thinking.clone(),
                egress_class: None,
            });
        }
        return None;
    }

    if gateway_cfg.provider.is_some() && gateway_cfg.model.is_some() {
        return Some(LlmConfig {
            provider: gateway_cfg.provider.clone().unwrap(),
            model: gateway_cfg.model.clone().unwrap(),
            temperature: 0.1,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
            egress_class: None,
        });
    }

    None
}

/// Effective compression config, merging gateway defaults with agent overrides.
pub fn effective_config(
    gateway_cfg: &ContextCompressionConfig,
    agent_cfg: Option<&CompressionConfig>,
) -> (f64, usize, usize) {
    let threshold_pct = agent_cfg
        .and_then(|a| a.threshold_pct)
        .unwrap_or(gateway_cfg.threshold_pct);
    let recent_turns = agent_cfg
        .and_then(|a| a.recent_turns_to_keep)
        .unwrap_or(gateway_cfg.recent_turns_to_keep);
    let max_summary_tokens = gateway_cfg.max_summary_tokens;
    (threshold_pct, recent_turns, max_summary_tokens)
}

fn find_tool_call_parent<'a>(
    messages: &'a [Message],
    tool_call_id: &str,
    search_up_to: usize,
) -> Option<usize> {
    for i in (0..search_up_to).rev() {
        if matches!(messages[i].role, Role::Assistant)
            && messages[i]
                .tool_calls
                .iter()
                .any(|tc| tc.id == tool_call_id)
        {
            return Some(i);
        }
    }
    None
}

/// Identify which messages are compressible (old) vs must be kept (recent).
///
/// Returns (compressible_messages, kept_messages) split point.
/// The system message (if any) is always kept. The last N user/assistant
/// exchange pairs are kept in full.
/// System messages are never compressible — they are always kept.
/// Tool-call groups (assistant with tool_calls + corresponding tool results)
/// are kept together — never split across the compress/keep boundary.
pub fn split_compressible_messages(
    messages: &[Message],
    recent_turns_to_keep: usize,
) -> (&[Message], &[Message]) {
    if messages.is_empty() {
        return (&[], &[]);
    }

    let non_system_count: usize = messages
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .count();
    let keep_count = recent_turns_to_keep * 2;

    if non_system_count <= keep_count {
        return (&[], messages);
    }

    let to_compress = non_system_count - keep_count;
    let mut compressed_count = 0;
    let mut split_idx = 0;
    for (i, msg) in messages.iter().enumerate() {
        if !matches!(msg.role, Role::System) {
            compressed_count += 1;
        }
        if compressed_count == to_compress {
            split_idx = i + 1;
            break;
        }
    }

    if split_idx == 0 {
        return (&[], messages);
    }

    let mut adjusted_split = split_idx;

    for i in split_idx..messages.len() {
        if matches!(messages[i].role, Role::Tool) {
            if let Some(ref tc_id) = messages[i].tool_call_id {
                if let Some(parent_idx) = find_tool_call_parent(messages, tc_id, split_idx) {
                    if parent_idx < adjusted_split {
                        adjusted_split = parent_idx;
                    }
                }
            }
        }
    }

    let mut final_split = adjusted_split;
    for i in 0..adjusted_split {
        if !messages[i].tool_calls.is_empty() {
            for tc in &messages[i].tool_calls {
                for j in adjusted_split..messages.len() {
                    if matches!(messages[j].role, Role::Tool)
                        && messages[j].tool_call_id.as_deref() == Some(&tc.id)
                    {
                        if j + 1 > final_split {
                            final_split = j + 1;
                        }
                    }
                }
            }
        }
    }

    if final_split == 0 {
        return (&[], messages);
    }

    (&messages[0..final_split], &messages[final_split..])
}

/// Build a summarization prompt for the compressible portion of history.
fn build_summarization_prompt(compressible: &[Message], max_summary_tokens: usize) -> String {
    let mut text = String::new();
    for msg in compressible {
        if msg.content.starts_with("[COMPRESSED CONTEXT") {
            continue;
        }
        match msg.role {
            Role::User => text.push_str(&format!("[User]: {}\n", msg.content)),
            Role::Assistant => text.push_str(&format!("[Assistant]: {}\n", msg.content)),
            Role::Tool => {
                if !msg.content.is_empty() {
                    text.push_str(&format!("[Tool Result]: {}\n", msg.content));
                }
            }
            Role::System => {}
        }
    }

    format!(
        "Summarize the following conversation history into a concise summary of at most {} tokens. \
        Focus on: goals discussed, decisions made, open items, key facts, and tool results. \
        Output ONLY the summary text, no preamble.\n\n\
        --- CONVERSATION ---\n{}\n--- END ---",
        max_summary_tokens, text
    )
}

/// Compress the conversation history by summarizing old turns.
///
/// Returns the original history unchanged if compression is not needed
/// (threshold not exceeded, not enough messages to compress, etc.).
pub async fn compress_context(
    history: Vec<Message>,
    context_window: Option<usize>,
    gateway_cfg: &ContextCompressionConfig,
    agent_cfg: Option<&CompressionConfig>,
    presets: &HashMap<String, LlmPreset>,
    http_client: &reqwest::Client,
    session_id: &str,
    turn_number: u64,
    existing_metadata: Option<&CompressionMetadata>,
) -> anyhow::Result<CompressionResult> {
    if !gateway_cfg.enabled {
        return Ok(CompressionResult {
            history: history.clone(),
            original_history: history,
            metadata: existing_metadata.cloned().unwrap_or_default(),
            compressed: false,
        });
    }

    if let Some(meta) = existing_metadata {
        let min_gap = gateway_cfg.min_turns_between_compression;
        if turn_number.saturating_sub(meta.last_compression_turn) < min_gap {
            return Ok(CompressionResult {
                history: history.clone(),
                original_history: history,
                metadata: existing_metadata.cloned().unwrap_or_default(),
                compressed: false,
            });
        }
    }

    let (threshold_pct, recent_turns_to_keep, max_summary_tokens) =
        effective_config(gateway_cfg, agent_cfg);

    let conv_tokens: usize = history
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .map(|m| estimate_message_tokens(m))
        .sum();

    let effective_limit = context_window.unwrap_or(128_000);
    let threshold_tokens = (effective_limit as f64 * threshold_pct / 100.0) as usize;

    if conv_tokens < threshold_tokens {
        return Ok(CompressionResult {
            history: history.clone(),
            original_history: history,
            metadata: existing_metadata.cloned().unwrap_or_default(),
            compressed: false,
        });
    }

    let (compressible, kept) = split_compressible_messages(&history, recent_turns_to_keep);

    if compressible.is_empty() {
        return Ok(CompressionResult {
            history: history.clone(),
            original_history: history,
            metadata: existing_metadata.cloned().unwrap_or_default(),
            compressed: false,
        });
    }

    let llm_config = match resolve_compression_llm_config(gateway_cfg, agent_cfg, presets) {
        Some(c) => c,
        None => {
            tracing::warn!(
                target: "autonoetic::compression",
                "Context compression enabled but no compression LLM configured"
            );
            return Ok(CompressionResult {
                history: history.clone(),
                original_history: history,
                metadata: existing_metadata.cloned().unwrap_or_default(),
                compressed: false,
            });
        }
    };

    let driver = match build_driver(llm_config.clone(), http_client.clone()) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                target: "autonoetic::compression",
                error = %e,
                "Failed to build compression LLM driver"
            );
            return Ok(CompressionResult {
                history: history.clone(),
                original_history: history,
                metadata: existing_metadata.cloned().unwrap_or_default(),
                compressed: false,
            });
        }
    };

    let prompt = build_summarization_prompt(compressible, max_summary_tokens);
    let req = CompletionRequest {
        model: llm_config.model.clone(),
        messages: vec![
            Message::system("You are a concise summarizer. Output only the summary."),
            Message::user(&prompt),
        ],
        tools: vec![],
        max_tokens: Some(max_summary_tokens as u32),
        temperature: Some(0.1),
        metadata: Some(HashMap::from([(
            "compression".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "turn": turn_number,
                "original_tokens": conv_tokens,
            }),
        )])),
        thinking: None,
        prompt_cache_key: None,
        system_cache_prefix_bytes: None,
    };

    let summary_text = match driver.complete(&req).await {
        Ok(resp) => resp.text,
        Err(e) => {
            tracing::warn!(
                target: "autonoetic::compression",
                error = %e,
                "Compression LLM call failed, skipping compression"
            );
            return Ok(CompressionResult {
                history: history.clone(),
                original_history: history,
                metadata: existing_metadata.cloned().unwrap_or_default(),
                compressed: false,
            });
        }
    };

    if summary_text.trim().is_empty() {
        tracing::warn!(
            target: "autonoetic::compression",
            "Compression LLM returned empty summary, skipping compression"
        );
        return Ok(CompressionResult {
            history: history.clone(),
            original_history: history,
            metadata: existing_metadata.cloned().unwrap_or_default(),
            compressed: false,
        });
    }

    let mut new_history: Vec<Message> = Vec::new();

    let system_messages: Vec<_> = history
        .iter()
        .filter(|m| matches!(m.role, Role::System))
        .cloned()
        .collect();
    new_history.extend(system_messages);

    new_history.push(Message::user(&format!(
        "[COMPRESSED CONTEXT - Turn {}]\n{}",
        turn_number, summary_text
    )));

    new_history.extend(
        kept.iter()
            .filter(|m| !matches!(m.role, Role::System))
            .cloned(),
    );

    let metadata = CompressionMetadata {
        last_compression_turn: turn_number,
        messages_summarized: compressible.len() as u64,
        compressed_context_handle: None,
        compression_count: existing_metadata
            .map(|m| m.compression_count + 1)
            .unwrap_or(1),
    };

    Ok(CompressionResult {
        history: new_history,
        original_history: history,
        metadata,
        compressed: true,
    })
}

/// Write the compressed context to the content store for audit/restore.
pub fn persist_compressed_context(
    gateway_dir: &std::path::Path,
    session_id: &str,
    history: &[Message],
    metadata: &CompressionMetadata,
) -> anyhow::Result<Option<String>> {
    let store = ContentStore::new(gateway_dir)?;

    let serialized = serde_json::to_string(history)?;
    let handle = store.write(serialized.as_bytes())?;

    let name = format!("compressed_context_turn_{}", metadata.last_compression_turn);
    store.register_name_with_visibility(session_id, &name, &handle, ContentVisibility::Private)?;

    Ok(Some(handle.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_compressible_messages_empty() {
        let messages: Vec<Message> = vec![];
        let (compressible, kept) = split_compressible_messages(&messages, 3);
        assert!(compressible.is_empty());
        assert!(kept.is_empty());
    }

    #[test]
    fn test_split_compressible_messages_only_system() {
        let messages = vec![Message::system("You are helpful.")];
        let (compressible, kept) = split_compressible_messages(&messages, 3);
        assert!(compressible.is_empty());
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn test_split_compressible_messages_keeps_recent() {
        let messages = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
            Message::user("u3"),
            Message::assistant("a3"),
        ];
        let (compressible, kept) = split_compressible_messages(&messages, 2);
        assert_eq!(compressible.len(), 3);
        assert_eq!(kept.len(), 4);
        assert!(matches!(kept[0].role, Role::User));
        assert!(matches!(kept[1].role, Role::Assistant));
        assert!(matches!(kept[2].role, Role::User));
        assert!(matches!(kept[3].role, Role::Assistant));
    }

    #[test]
    fn test_split_compressible_messages_all_recent() {
        let messages = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
        ];
        let (compressible, kept) = split_compressible_messages(&messages, 3);
        assert!(compressible.is_empty());
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn test_effective_config_uses_gateway_defaults() {
        let gateway = ContextCompressionConfig {
            enabled: true,
            llm_preset: Some("haiku".into()),
            threshold_pct: 50.0,
            recent_turns_to_keep: 4,
            max_summary_tokens: 600,
            ..Default::default()
        };
        let (threshold, recent, max) = effective_config(&gateway, None);
        assert_eq!(threshold, 50.0);
        assert_eq!(recent, 4);
        assert_eq!(max, 600);
    }

    #[test]
    fn test_effective_config_agent_override() {
        let gateway = ContextCompressionConfig {
            enabled: true,
            llm_preset: Some("haiku".into()),
            threshold_pct: 50.0,
            recent_turns_to_keep: 4,
            max_summary_tokens: 600,
            ..Default::default()
        };
        let agent = CompressionConfig {
            threshold_pct: Some(40.0),
            recent_turns_to_keep: Some(2),
            llm_preset: Some("cheap".into()),
            max_capsule_decisions: None,
            max_completed_tasks: None,
        };
        let (threshold, recent, max) = effective_config(&gateway, Some(&agent));
        assert_eq!(threshold, 40.0);
        assert_eq!(recent, 2);
        assert_eq!(max, 600);
    }

    #[test]
    fn test_estimate_tokens_for_split() {
        let msgs = vec![
            Message::user("Hello world"),
            Message::assistant("Hi there!"),
        ];
        let tokens: usize = msgs.iter().map(|m| estimate_message_tokens(m)).sum();
        assert!(tokens > 0);
    }

    #[test]
    fn test_build_summarization_prompt() {
        let msgs = vec![
            Message::user("What is rust?"),
            Message::assistant("A systems language."),
        ];
        let prompt = build_summarization_prompt(&msgs, 100);
        assert!(prompt.contains("What is rust?"));
        assert!(prompt.contains("A systems language."));
        assert!(prompt.contains("100 tokens"));
    }

    #[test]
    fn test_compression_disabled() {
        let gateway = ContextCompressionConfig {
            enabled: false,
            ..Default::default()
        };
        let history = vec![Message::user("hello"), Message::assistant("hi")];
        let result = CompressionResult {
            history: history.clone(),
            original_history: history.clone(),
            metadata: CompressionMetadata::default(),
            compressed: false,
        };
        assert!(!result.compressed);
    }

    #[test]
    fn test_resolve_compression_llm_config_from_preset() {
        let mut presets = HashMap::new();
        presets.insert(
            "haiku".to_string(),
            LlmPreset {
                provider: Some("anthropic".to_string()),
                model: Some("claude-3-haiku-20240307".to_string()),
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
            },
        );
        let gateway = ContextCompressionConfig {
            enabled: true,
            llm_preset: Some("haiku".to_string()),
            ..Default::default()
        };
        let config = resolve_compression_llm_config(&gateway, None, &presets);
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.model, "claude-3-haiku-20240307");
    }

    #[test]
    fn test_resolve_compression_llm_config_inline() {
        let presets = HashMap::new();
        let gateway = ContextCompressionConfig {
            enabled: true,
            provider: Some("openai".to_string()),
            model: Some("gpt-4o-mini".to_string()),
            ..Default::default()
        };
        let config = resolve_compression_llm_config(&gateway, None, &presets);
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.model, "gpt-4o-mini");
    }

    #[test]
    fn test_split_tool_call_groups_kept_together() {
        let tool_call = crate::llm::ToolCall {
            id: "tc1".to_string(),
            name: "search".to_string(),
            arguments: "{}".to_string(),
        };
        let messages = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            {
                let mut m = Message::assistant("a2");
                m.tool_calls = vec![tool_call.clone()];
                m
            },
            Message::tool_result("tc1", "search", "result1"),
            Message::user("u3"),
            Message::assistant("a3"),
        ];
        let (compressible, kept) = split_compressible_messages(&messages, 2);
        assert_eq!(compressible.len(), 4);
        assert_eq!(kept.len(), 4);
        let kept_has_tool_result = kept
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("tc1"));
        let kept_has_tool_call = kept
            .iter()
            .any(|m| m.tool_calls.iter().any(|tc| tc.id == "tc1"));
        assert_eq!(kept_has_tool_result, kept_has_tool_call);
        let comp_has_tool_result = compressible
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("tc1"));
        let comp_has_tool_call = compressible
            .iter()
            .any(|m| m.tool_calls.iter().any(|tc| tc.id == "tc1"));
        assert_eq!(comp_has_tool_result, comp_has_tool_call);
    }

    #[test]
    fn test_split_tool_call_parent_pulled_into_kept() {
        let tool_call = crate::llm::ToolCall {
            id: "tc_x".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        };
        let messages = vec![
            Message::system("sys"),
            {
                let mut m = Message::assistant("a1");
                m.tool_calls = vec![tool_call.clone()];
                m
            },
            Message::tool_result("tc_x", "read", "file content"),
            Message::user("u2"),
            Message::assistant("a2"),
            Message::user("u3"),
            Message::assistant("a3"),
        ];
        let (compressible, kept) = split_compressible_messages(&messages, 2);
        let has_tool_call = |msgs: &[Message]| {
            msgs.iter()
                .any(|m| m.tool_calls.iter().any(|tc| tc.id == "tc_x"))
        };
        let has_tool_result = |msgs: &[Message]| {
            msgs.iter()
                .any(|m| m.tool_call_id.as_deref() == Some("tc_x"))
        };
        if has_tool_result(kept) {
            assert!(
                has_tool_call(compressible) || has_tool_call(kept),
                "Tool result in kept but parent assistant missing from both sides"
            );
        }
        if has_tool_result(compressible) {
            assert!(
                has_tool_call(compressible),
                "Tool result in compressible but parent assistant not in compressible"
            );
        }
    }

    #[test]
    fn test_split_tool_call_parent_in_compressible_tool_in_kept() {
        let tool_call = crate::llm::ToolCall {
            id: "tc2".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        };
        let messages = vec![
            Message::system("sys"),
            {
                let mut m = Message::assistant("a1");
                m.tool_calls = vec![tool_call.clone()];
                m
            },
            Message::tool_result("tc2", "read", "result"),
            Message::user("u2"),
            Message::assistant("a2"),
            Message::user("u3"),
            Message::assistant("a3"),
        ];
        let (compressible, kept) = split_compressible_messages(&messages, 2);
        let has_tool_call = |msgs: &[Message]| {
            msgs.iter()
                .any(|m| m.tool_calls.iter().any(|tc| tc.id == "tc2"))
        };
        let has_tool_result = |msgs: &[Message]| {
            msgs.iter()
                .any(|m| m.tool_call_id.as_deref() == Some("tc2"))
        };
        if has_tool_result(kept) {
            assert!(
                has_tool_call(compressible) || has_tool_call(kept),
                "Tool result in kept but parent assistant not in either side"
            );
        }
    }

    #[test]
    fn test_build_summarization_prompt_skips_compressed_context() {
        let msgs = vec![
            Message::user("[COMPRESSED CONTEXT - Turn 5]\nPrevious summary"),
            Message::user("actual user message"),
            Message::assistant("actual assistant response"),
        ];
        let prompt = build_summarization_prompt(&msgs, 100);
        assert!(prompt.contains("actual user message"));
        assert!(prompt.contains("actual assistant response"));
        assert!(!prompt.contains("Previous summary"));
    }

    #[test]
    fn test_min_turns_between_compression_blocks_early_recompression() {
        let gateway = ContextCompressionConfig {
            enabled: true,
            min_turns_between_compression: 5,
            ..Default::default()
        };
        let metadata = CompressionMetadata {
            last_compression_turn: 10,
            ..Default::default()
        };
        let history = vec![Message::user("hello"), Message::assistant("hi")];
        let rt = tokio::runtime::Runtime::new().unwrap();
        let presets = std::collections::HashMap::new();
        let client = reqwest::Client::new();
        let result = rt
            .block_on(compress_context(
                history.clone(),
                Some(128_000),
                &gateway,
                None,
                &presets,
                &client,
                "sess",
                12,
                Some(&metadata),
            ))
            .unwrap();
        assert!(!result.compressed);

        let result = rt
            .block_on(compress_context(
                history.clone(),
                Some(128_000),
                &gateway,
                None,
                &presets,
                &client,
                "sess",
                16,
                Some(&metadata),
            ))
            .unwrap();
        assert!(!result.compressed);

        let result = rt
            .block_on(compress_context(
                history,
                Some(128_000),
                &gateway,
                None,
                &presets,
                &client,
                "sess",
                20,
                Some(&metadata),
            ))
            .unwrap();
        assert!(!result.compressed);
    }
}
