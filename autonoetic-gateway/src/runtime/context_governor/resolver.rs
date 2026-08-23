//! Context window resolution.
//!
//! Resolves the effective context window from manifest, env vars, or
//! provider catalog. Also includes static lookup for common models.

use crate::runtime::llm_preset_resolver::context_window_tokens_from_gateway_config;
use crate::runtime::local_model_context::LocalModelContextCache;
use crate::runtime::openrouter_catalog::OpenRouterCatalog;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;

/// Resolve from manifest or env var.
pub fn resolve_context_window_tokens(manifest: &AgentManifest) -> Option<u32> {
    if let Some(cfg) = &manifest.llm_config {
        if let Some(w) = cfg.context_window_tokens {
            return Some(w);
        }
    }
    crate::runtime::budget_tracker::llm_context_window_env_tokens()
}

/// Manifest/env first, then gateway `llm_presets` via `llm_preset_mapping`.
/// If still unknown and provider is OpenRouter, use the public models API cache.
/// For other providers, falls back to static table and local server probe cache.
pub async fn resolve_context_window_for_run(
    manifest: &AgentManifest,
    model: &str,
    catalog: Option<&Arc<OpenRouterCatalog>>,
    local_context: Option<&Arc<LocalModelContextCache>>,
    gateway_config: Option<&GatewayConfig>,
) -> Option<u32> {
    if let Some(w) = resolve_context_window_tokens(manifest) {
        return Some(w);
    }
    if let Some(config) = gateway_config {
        if let Some(w) =
            context_window_tokens_from_gateway_config(&manifest.agent.id, config)
        {
            return Some(w);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{
        AgentIdentity, AgentManifest, ExecutionMode, RuntimeDeclaration, SandboxNetworkPolicy,
    };
    use autonoetic_types::config::{GatewayConfig, LlmPreset};
    use std::collections::HashMap;

    fn empty_llm_preset() -> LlmPreset {
        LlmPreset {
            provider: None,
            model: None,
            temperature: None,
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
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        }
    }

    fn minimal_manifest(agent_id: &str, context_window_tokens: Option<u32>) -> AgentManifest {
        AgentManifest {
            remote_access: None,
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
                id: agent_id.to_string(),
                name: agent_id.to_string(),
                description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
            capabilities: vec![],
            llm_overrides: None,
            llm_preset: None,
            llm_config: context_window_tokens.map(|w| autonoetic_types::agent::LlmConfig {
                provider: "llamacpp".to_string(),
                model: "qwen".to_string(),
                temperature: 0.2,
                fallback_provider: None,
                fallback_model: None,
                chat_only: false,
                context_window_tokens: Some(w),
                base_url: None,
                api_key_env: None,
                routing_preset: None,
                thinking: None,
                egress_class: None,
                request_timeout_secs: None,
                ttfb_timeout_secs: None,
            }),
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: ExecutionMode::Reasoning,
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            excluded_tools: vec![],
            sections: Vec::new(),
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: SandboxNetworkPolicy::default(),
            egress: None,
        }
    }

    #[tokio::test]
    async fn resolve_context_window_uses_gateway_preset_when_manifest_omits_it() {
        let mut presets = HashMap::new();
        presets.insert(
            "default".to_string(),
            LlmPreset {
                provider: Some("llamacpp".to_string()),
                model: Some("qwen".to_string()),
                context_window_tokens: Some(114_688),
                egress_class: None,
                ..empty_llm_preset()
            },
        );
        let mut mapping = HashMap::new();
        mapping.insert("planner".to_string(), "default".to_string());
        let gateway = GatewayConfig {
            llm_presets: presets,
            llm_preset_mapping: mapping,
            ..GatewayConfig::default()
        };
        let manifest = minimal_manifest("planner.default", None);

        let resolved = resolve_context_window_for_run(
            &manifest,
            "qwen",
            None,
            None,
            Some(&gateway),
        )
        .await;

        assert_eq!(resolved, Some(114_688));
    }

    #[tokio::test]
    async fn resolve_context_window_manifest_wins_over_gateway_preset() {
        let mut presets = HashMap::new();
        presets.insert(
            "default".to_string(),
            LlmPreset {
                context_window_tokens: Some(114_688),
                egress_class: None,
                ..empty_llm_preset()
            },
        );
        let mut mapping = HashMap::new();
        mapping.insert("planner".to_string(), "default".to_string());
        let gateway = GatewayConfig {
            llm_presets: presets,
            llm_preset_mapping: mapping,
            ..GatewayConfig::default()
        };
        let manifest = minimal_manifest("planner.default", Some(32_768));

        let resolved = resolve_context_window_for_run(
            &manifest,
            "qwen",
            None,
            None,
            Some(&gateway),
        )
        .await;

        assert_eq!(resolved, Some(32_768));
    }
}
