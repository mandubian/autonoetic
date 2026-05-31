//! Context window resolution.
//!
//! Resolves the effective context window from manifest, env vars, or
//! provider catalog. Also includes static lookup for common models.

use crate::runtime::local_model_context::LocalModelContextCache;
use crate::runtime::openrouter_catalog::OpenRouterCatalog;
use autonoetic_types::agent::AgentManifest;
use std::sync::Arc;

/// Resolve from manifest or env var.
pub fn resolve_context_window_tokens(manifest: &AgentManifest) -> Option<u32> {
    if let Some(cfg) = &manifest.llm_config {
        if let Some(w) = cfg.context_window_tokens {
            return Some(w);
        }
    }
    std::env::var("AUTONOETIC_LLM_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Manifest/env first; if still unknown and provider is OpenRouter, use the
/// public models API cache. For other providers, falls back to static table.
pub async fn resolve_context_window_for_run(
    manifest: &AgentManifest,
    model: &str,
    catalog: Option<&Arc<OpenRouterCatalog>>,
    local_context: Option<&Arc<LocalModelContextCache>>,
) -> Option<u32> {
    if let Some(w) = resolve_context_window_tokens(manifest) {
        return Some(w);
    }
    if let Some(w) = static_context_window(model) {
        return Some(w);
    }
    if let Some(cache) = local_context {
        if let Some(base_url) = manifest
            .llm_config
            .as_ref()
            .and_then(|c| c.base_url.as_deref())
            .filter(|u| !u.is_empty())
        {
            if let Some(w) = cache.get(base_url, model) {
                return Some(w);
            }
        }
    }
    let use_openrouter = manifest
        .llm_config
        .as_ref()
        .map(|c| c.provider.eq_ignore_ascii_case("openrouter"))
        .unwrap_or(false);
    if !use_openrouter {
        return None;
    }
    match catalog {
        Some(cat) => cat.context_length_for_model(model).await.map(|v| v as u32),
        None => None,
    }
}

/// Static lookup table for common models that don't go through OpenRouter catalog.
pub fn static_context_window(model: &str) -> Option<u32> {
    let model_lower = model.to_lowercase();
    // OpenAI models
    if model_lower.starts_with("gpt-4o") {
        return Some(128_000);
    }
    if model_lower.starts_with("gpt-4-turbo") {
        return Some(128_000);
    }
    if model_lower.starts_with("gpt-4-") && model_lower.contains("1106") {
        return Some(128_000);
    }
    if model_lower.starts_with("gpt-4") && !model_lower.contains("turbo") {
        return Some(8_192);
    }
    if model_lower.starts_with("gpt-3.5-turbo") {
        return Some(16_385);
    }
    if model_lower.starts_with("o1") || model_lower.starts_with("o3") || model_lower.starts_with("o4") {
        return Some(200_000);
    }
    // Anthropic models
    if model_lower.contains("claude") {
        if model_lower.contains("sonnet-4") || model_lower.contains("opus-4") || model_lower.contains("haiku-4") {
            return Some(200_000);
        }
        if model_lower.contains("sonnet") || model_lower.contains("opus") {
            return Some(200_000);
        }
        if model_lower.contains("haiku") {
            return Some(200_000);
        }
    }
    // Gemini models
    if model_lower.starts_with("gemini") {
        if model_lower.contains("2.0") || model_lower.contains("2.5") || model_lower.contains("2.0-flash") {
            return Some(1_048_576);
        }
        if model_lower.contains("1.5") {
            return Some(1_048_576);
        }
        return Some(128_000);
    }
    None
}

/// Maps provider prompt (`input`) token count to % of a declared context window.
pub fn input_tokens_as_context_pct(input_tokens: u64, context_window: Option<u32>) -> Option<f32> {
    let w = f64::from(context_window?);
    if w <= 0.0 {
        return None;
    }
    let pct = (input_tokens as f64 / w) * 100.0;
    Some(pct.min(9999.0) as f32)
}
