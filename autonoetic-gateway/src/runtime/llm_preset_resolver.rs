//! Resolves LLM preset names to concrete configurations.
//!
//! A preset can be either **fixed** (concrete provider/model) or **routing**
//! (dynamic selection from fixed presets at call time). This module handles
//! the resolution of both kinds and provides the resolved model list needed
//! by the router.

use crate::runtime::model_router::ResolvedModelEntry;
use autonoetic_types::agent::LlmConfig;
use autonoetic_types::config::{CapabilityTier, LlmPreset, RoutingPresetConfig};
use std::collections::HashMap;

/// Resolves a fixed preset to its concrete LlmConfig.
pub fn resolve_fixed_preset(preset: &LlmPreset) -> Option<LlmConfig> {
    Some(LlmConfig {
        provider: preset.provider.clone()?,
        model: preset.model.clone()?,
        temperature: preset.temperature.unwrap_or(0.2),
        fallback_provider: preset.fallback_provider.clone(),
        fallback_model: preset.fallback_model.clone(),
        chat_only: preset.chat_only.unwrap_or(false),
        context_window_tokens: preset.context_window_tokens,
        base_url: preset.base_url.clone(),
        api_key_env: preset.api_key_env.clone(),
        routing_preset: None,
        thinking: preset.thinking.clone(),
    })
}

/// Resolves a fixed preset to a ResolvedModelEntry (for use in routing model lists).
pub fn resolve_fixed_preset_as_entry(name: &str, preset: &LlmPreset) -> Option<ResolvedModelEntry> {
    Some(ResolvedModelEntry {
        preset_name: name.to_string(),
        config: resolve_fixed_preset(preset)?,
        tier: preset.tier.unwrap_or(CapabilityTier::Economy),
    })
}

/// Resolves a classifier preset name to a concrete LlmConfig.
pub fn resolve_classifier_config(
    classifier_preset_name: &str,
    presets: &HashMap<String, LlmPreset>,
) -> Option<LlmConfig> {
    let cp = presets.get(classifier_preset_name)?;
    // Classifier must be a fixed preset
    if cp.routing.is_some() {
        return None;
    }
    resolve_fixed_preset(cp)
}

/// Builds the resolved model list from a routing preset's models field.
/// Each entry in `routing.models` is a preset name that must resolve to a fixed preset.
pub fn resolve_model_list(
    routing: &RoutingPresetConfig,
    presets: &HashMap<String, LlmPreset>,
) -> Vec<ResolvedModelEntry> {
    routing
        .models
        .iter()
        .filter_map(|model_preset_name| {
            let mp = presets.get(model_preset_name)?;
            // Skip routing presets in the models list — only fixed presets allowed
            if mp.routing.is_some() {
                return None;
            }
            resolve_fixed_preset_as_entry(model_preset_name, mp)
        })
        .collect()
}

/// Checks if a preset is a routing preset (has `routing` field set).
pub fn is_routing_preset(preset: &LlmPreset) -> bool {
    preset.routing.is_some()
}

/// Checks if a preset is fixed (has `provider` and `model` set, no `routing`).
pub fn is_fixed_preset(preset: &LlmPreset) -> bool {
    preset.provider.is_some() && preset.model.is_some() && preset.routing.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::config::{
        CapabilityTier, ClassifierRoutingConfig, DeterministicRoutingConfig, HybridRoutingConfig,
        RoutingPresetConfig, RoutingStrategy,
    };

    fn fixed_preset(provider: &str, model: &str, tier: CapabilityTier) -> LlmPreset {
        LlmPreset {
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            temperature: Some(0.2),
            fallback_provider: None,
            fallback_model: None,
            chat_only: None,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            thinking: None,
            tier: Some(tier),
            cost: None,
            latency: None,
            routing: None,
        }
    }

    fn routing_preset(models: Vec<&str>) -> LlmPreset {
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
            routing: Some(RoutingPresetConfig {
                strategy: RoutingStrategy::Deterministic,
                models: models.into_iter().map(|s| s.to_string()).collect(),
                classifier_preset: None,
                deterministic: DeterministicRoutingConfig::default(),
                classifier: ClassifierRoutingConfig::default(),
                hybrid: HybridRoutingConfig::default(),
            }),
        }
    }

    #[test]
    fn test_is_fixed_preset() {
        let p = fixed_preset("openai", "gpt-4", CapabilityTier::Premium);
        assert!(is_fixed_preset(&p));
        assert!(!is_routing_preset(&p));
    }

    #[test]
    fn test_is_routing_preset() {
        let p = routing_preset(vec!["haiku", "sonnet"]);
        assert!(is_routing_preset(&p));
        assert!(!is_fixed_preset(&p));
    }

    #[test]
    fn test_resolve_fixed_preset() {
        let p = fixed_preset("anthropic", "claude-sonnet", CapabilityTier::Standard);
        let cfg = resolve_fixed_preset(&p).unwrap();
        assert_eq!(cfg.provider, "anthropic");
        assert_eq!(cfg.model, "claude-sonnet");
        assert_eq!(cfg.temperature, 0.2);
    }

    #[test]
    fn test_resolve_model_list_skips_routing_presets() {
        let mut presets = HashMap::new();
        presets.insert(
            "haiku".to_string(),
            fixed_preset("anthropic", "haiku-3", CapabilityTier::Economy),
        );
        presets.insert(
            "sonnet".to_string(),
            fixed_preset("anthropic", "sonnet-4", CapabilityTier::Standard),
        );
        presets.insert("smart".to_string(), routing_preset(vec!["haiku", "sonnet"]));

        let routing = RoutingPresetConfig {
            strategy: RoutingStrategy::Deterministic,
            models: vec!["haiku".to_string(), "smart".to_string()], // smart is a routing preset — should be skipped
            classifier_preset: None,
            deterministic: DeterministicRoutingConfig::default(),
            classifier: ClassifierRoutingConfig::default(),
            hybrid: HybridRoutingConfig::default(),
        };

        let resolved = resolve_model_list(&routing, &presets);
        assert_eq!(resolved.len(), 1); // only haiku, smart is skipped
        assert_eq!(resolved[0].preset_name, "haiku");
    }

    #[test]
    fn test_resolve_classifier_config_rejects_routing_preset() {
        let mut presets = HashMap::new();
        presets.insert("smart".to_string(), routing_preset(vec!["haiku"]));

        let result = resolve_classifier_config("smart", &presets);
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_fixed_preset_as_entry() {
        let p = fixed_preset("openai", "gpt-4o", CapabilityTier::Premium);
        let entry = resolve_fixed_preset_as_entry("gpt4o", &p).unwrap();
        assert_eq!(entry.preset_name, "gpt4o");
        assert_eq!(entry.config.provider, "openai");
        assert_eq!(entry.config.model, "gpt-4o");
        assert_eq!(entry.tier, CapabilityTier::Premium);
    }
}
