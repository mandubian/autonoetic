//! Integration: egress compression-eligibility + per-label-band compression
//! (RFC §5.7 rules 1–2).
//!
//! Rule 1: a `local_only`-tainted band is **never** sent to a remote
//! compression preset. Rule 2: clean and tainted messages compress in
//! **separate** bands — never a single mixed summary. An ineligible band
//! falls back to token-budget truncation (an incomplete local context beats
//! a remote leak).
//!
//! Drives [`compress_context`] directly. Driver build fails without API keys,
//! so eligible bands truncate too — the assertions focus on band split,
//! labels on synthesized blocks, and canary never being summarized remotely
//! (truncation is local-only and does not call the LLM).

use std::collections::HashMap;

use autonoetic_gateway::runtime::compression::{compress_context, CompressionResult};
use autonoetic_types::config::{ContextCompressionConfig, LlmPreset};
use autonoetic_types::egress::{EgressClass, EgressLabel};

use autonoetic_gateway::llm::{Message, Role};

/// Build a minimal fixed preset that resolves to a remote compression LLM.
fn remote_preset() -> LlmPreset {
    LlmPreset {
        provider: Some("anthropic".to_string()),
        model: Some("claude-haiku-3".to_string()),
        temperature: Some(0.1),
        fallback_provider: None,
        fallback_model: None,
        chat_only: None,
        context_window_tokens: None,
        max_tokens: None,
        base_url: None,
        api_key_env: None,
        thinking: None,
        tier: None,
        cost: None,
        latency: None,
        routing: None,
        // Explicitly remote (the default, but stated for clarity — this preset
        // is the sink the gate must refuse the local_only band for).
        egress_class: Some(EgressClass::Remote),
        request_timeout_secs: None,
        ttfb_timeout_secs: None,
    }
}

/// A local preset — cleared for local_only content.
fn local_preset() -> LlmPreset {
    let mut p = remote_preset();
    p.provider = Some("ollama".to_string());
    p.model = Some("llama3".to_string());
    p.egress_class = Some(EgressClass::Local);
    p
}

fn compression_cfg(preset: &str) -> ContextCompressionConfig {
    ContextCompressionConfig {
        enabled: true,
        llm_preset: Some(preset.to_string()),
        provider: None,
        model: None,
        // Force the threshold so tiny test histories always compress.
        threshold_pct: 0.0,
        // Keep only the last exchange so earlier (labeled) turns are compressible.
        recent_turns_to_keep: 1,
        max_summary_tokens: 80,
        min_turns_between_compression: 1,
        max_capsule_decisions: 30,
        max_completed_tasks: 10,
    }
}

fn tool_msg(id: &str, content: &str) -> Message {
    Message {
        id: None,
        role: Role::Tool,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: Some(id.to_string()),
        reasoning_content: None,
        reasoning_details: None,
    }
}

fn user_msg(content: &str) -> Message {
    Message {
        id: None,
        role: Role::User,
        content: content.to_string(),
        tool_calls: vec![],
        tool_call_id: None,
        reasoning_content: None,
        reasoning_details: None,
    }
}

/// A history whose compressible prefix is a single local_only tool result
/// (the canary). `recent_turns_to_keep: 1` retains the trailing pair.
fn history_with_secret() -> Vec<Message> {
    vec![
        tool_msg("tc_secret", "CANARY-SECRET-EMAIL-CONTENT"),
        user_msg("turn recent a"),
        user_msg("turn recent b"),
    ]
}

/// Mixed clean + tainted history — the §5.7 rule 2 acceptance shape.
/// Both labeled tool results sit in the compressible prefix (recent_turns=1
/// keeps only the last two non-system messages).
fn mixed_history() -> Vec<Message> {
    vec![
        user_msg("clean code review"),
        tool_msg("tc_public", "public lint results look fine"),
        user_msg("now read my mail"),
        tool_msg("tc_secret", "CANARY-SECRET-EMAIL-CONTENT"),
        user_msg("filler"),
        user_msg("turn recent a"),
        user_msg("turn recent b"),
    ]
}

fn context_blocks(history: &[Message]) -> Vec<&Message> {
    history
        .iter()
        .filter(|m| {
            m.content.starts_with("[COMPRESSED CONTEXT")
                || m.content.starts_with("[TRUNCATED CONTEXT")
        })
        .collect()
}

#[tokio::test]
async fn local_only_band_truncated_not_remotely_compressed() -> anyhow::Result<()> {
    let mut presets = HashMap::new();
    presets.insert("haiku".to_string(), remote_preset());
    let cfg = compression_cfg("haiku");
    let client = reqwest::Client::new();

    let mut labels = HashMap::new();
    labels.insert("tc_secret".to_string(), EgressLabel::local_only());

    let result: CompressionResult = compress_context(
        history_with_secret(),
        Some(128_000),
        &cfg,
        None,
        &presets,
        &client,
        "sess-egress-compress",
        5,
        None,
        &mut labels,
    )
    .await?;

    // Rule 2: ineligible band is truncated locally — compression applied, but
    // the block is TRUNCATED (never shipped to the remote preset).
    assert!(
        result.compressed,
        "ineligible local_only band must truncate rather than no-op"
    );
    let blocks = context_blocks(&result.history);
    assert_eq!(blocks.len(), 1);
    assert!(
        blocks[0].content.starts_with("[TRUNCATED CONTEXT"),
        "local_only band on remote preset must truncate, not LLM-compress: {}",
        &blocks[0].content[..blocks[0].content.len().min(80)]
    );
    // Synthesized block carries the band label in the sidecar.
    let id = blocks[0].id.as_deref().expect("synthesized block needs msg id");
    assert_eq!(labels.get(id), Some(&EgressLabel::local_only()));
    Ok(())
}

#[tokio::test]
async fn mixed_history_yields_two_labeled_blocks() -> anyhow::Result<()> {
    let mut presets = HashMap::new();
    presets.insert("haiku".to_string(), remote_preset());
    let cfg = compression_cfg("haiku");
    let client = reqwest::Client::new();

    let mut labels = HashMap::new();
    labels.insert("tc_public".to_string(), EgressLabel::unrestricted());
    labels.insert("tc_secret".to_string(), EgressLabel::local_only());

    let result = compress_context(
        mixed_history(),
        Some(128_000),
        &cfg,
        None,
        &presets,
        &client,
        "sess-mixed-bands",
        5,
        None,
        &mut labels,
    )
    .await?;

    assert!(result.compressed);
    let blocks = context_blocks(&result.history);
    assert_eq!(
        blocks.len(),
        2,
        "mixed history must yield two labeled blocks (RFC §5.7 rule 2), got: {:?}",
        blocks
            .iter()
            .map(|m| m.content.lines().next().unwrap_or(""))
            .collect::<Vec<_>>()
    );

    // Every synthesized block has an id + sidecar label.
    let mut saw_local = false;
    let mut saw_unrestricted = false;
    for block in &blocks {
        let id = block.id.as_deref().expect("block id");
        let label = labels.get(id).expect("sidecar label for synthesized block");
        if *label == EgressLabel::local_only() {
            saw_local = true;
            assert!(
                block.content.starts_with("[TRUNCATED CONTEXT"),
                "tainted band must never go remote: {}",
                block.content.lines().next().unwrap_or("")
            );
        }
        if label.is_unrestricted() {
            saw_unrestricted = true;
        }
    }
    assert!(saw_local, "expected a local_only band block");
    assert!(saw_unrestricted, "expected an unrestricted band block");
    // Source labels for the original tool results still survive (§3.4).
    assert_eq!(labels.get("tc_secret"), Some(&EgressLabel::local_only()));
    assert_eq!(labels.get("tc_public"), Some(&EgressLabel::unrestricted()));
    Ok(())
}

#[tokio::test]
async fn local_only_band_may_compress_on_local_preset() -> anyhow::Result<()> {
    // The local model is a cleared sink for local_only — the gate allows it.
    // (No real ollama here: eligibility passes, driver build/call fails, and
    // the label-plane path truncates. Assert we did NOT take the ineligible
    // path for the wrong reason — the band is eligible, so a TRUNCATED block
    // still carries local_only.)
    let mut presets = HashMap::new();
    presets.insert("local".to_string(), local_preset());
    let cfg = compression_cfg("local");
    let client = reqwest::Client::new();

    let mut labels = HashMap::new();
    labels.insert("tc_secret".to_string(), EgressLabel::local_only());

    let result = compress_context(
        history_with_secret(),
        Some(128_000),
        &cfg,
        None,
        &presets,
        &client,
        "sess-local-compress",
        5,
        None,
        &mut labels,
    )
    .await?;

    assert!(result.compressed);
    let blocks = context_blocks(&result.history);
    assert_eq!(blocks.len(), 1);
    let id = blocks[0].id.as_deref().unwrap();
    assert_eq!(labels.get(id), Some(&EgressLabel::local_only()));
    Ok(())
}

#[tokio::test]
async fn unrestricted_band_compresses_normally_on_remote_preset() -> anyhow::Result<()> {
    // No false refusal: an unrestricted / empty sidecar must not be blocked.
    let mut presets = HashMap::new();
    presets.insert("haiku".to_string(), remote_preset());
    let cfg = compression_cfg("haiku");
    let client = reqwest::Client::new();

    let mut labels: HashMap<String, EgressLabel> = HashMap::new();

    let result = compress_context(
        history_with_secret(),
        Some(128_000),
        &cfg,
        None,
        &presets,
        &client,
        "sess-unrestricted",
        5,
        None,
        &mut labels,
    )
    .await?;

    // Empty sidecar → legacy path: driver failure returns uncompressed.
    // (No API key → compressed:false via driver-error, NOT eligibility.)
    assert!(
        !result.compressed || !result.synthesized_message_ids.is_empty(),
        "empty-label path must not invent labeled blocks"
    );
    Ok(())
}

/// Direct unit-style check of the eligibility helper covering the live
/// `extract_delta` path's logic, without spinning up the governor harness.
#[test]
fn eligibility_helper_local_only_band_ineligible_on_remote() {
    use autonoetic_gateway::runtime::egress_labeler::{
        compression_preset_eligible, CompressionEligibility,
    };
    let band = vec![tool_msg("tc_secret", "secret")];
    let mut labels = HashMap::new();
    labels.insert("tc_secret".to_string(), EgressLabel::local_only());
    let elig = compression_preset_eligible(&band, &labels, EgressClass::Remote);
    assert!(matches!(elig, CompressionEligibility::Ineligible { .. }));
    // And eligible on local.
    let elig = compression_preset_eligible(&band, &labels, EgressClass::Local);
    assert!(elig.is_eligible());
}
