//! Gateway configuration loading.

use autonoetic_types::config::GatewayConfig;
use std::path::Path;

/// Load a `GatewayConfig` from a YAML file on disk.
///
/// Falls back to `GatewayConfig::default()` if the path does not exist.
pub fn load_config(path: &Path) -> anyhow::Result<GatewayConfig> {
    if path.exists() {
        let contents = std::fs::read_to_string(path)?;
        let mut config: GatewayConfig = serde_yaml::from_str(&contents)?;
        // Canonicalize agents_dir to absolute path so all components resolve to the same location
        config.agents_dir = config
            .agents_dir
            .canonicalize()
            .unwrap_or_else(|_| config.agents_dir.clone());
        apply_role_mapping_fallbacks(&mut config);
        Ok(config)
    } else {
        tracing::warn!("Config not found at {}, using defaults", path.display());
        Ok(GatewayConfig::default())
    }
}

/// Populate role-keyed config fields from `llm_preset_mapping` when they
/// are not set explicitly. This lets operators centralize preset choice
/// for cross-cutting roles (e.g. `context_compression`) alongside the
/// per-agent role mappings.
fn apply_role_mapping_fallbacks(config: &mut GatewayConfig) {
    if config.context_compression.llm_preset.is_none() {
        if let Some(preset) = config.llm_preset_mapping.get("context_compression") {
            config.context_compression.llm_preset = Some(preset.clone());
        }
    }
}

/// Resolve and load the persona text from the config.
///
/// Resolution order:
/// 1. Explicit `persona_path` in config (relative paths resolve from `config_dir`)
/// 2. Default `persona.md` next to the config file
///
/// Returns `None` if no persona file exists (not an error).
pub fn load_persona(config: &GatewayConfig, config_dir: &Path) -> Option<String> {
    let path = if let Some(ref explicit) = config.persona_path {
        if explicit.is_absolute() {
            explicit.clone()
        } else {
            config_dir.join(explicit)
        }
    } else {
        config_dir.join("persona.md")
    };

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                None
            } else {
                tracing::info!("Loaded persona from {}", path.display());
                Some(trimmed.to_string())
            }
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_mapping_fallback_populates_context_compression_preset() {
        let mut config = GatewayConfig::default();
        config
            .llm_preset_mapping
            .insert("context_compression".to_string(), "haiku".to_string());
        assert!(config.context_compression.llm_preset.is_none());
        apply_role_mapping_fallbacks(&mut config);
        assert_eq!(
            config.context_compression.llm_preset.as_deref(),
            Some("haiku")
        );
    }

    #[test]
    fn role_mapping_fallback_does_not_override_explicit_preset() {
        let mut config = GatewayConfig::default();
        config.context_compression.llm_preset = Some("explicit".to_string());
        config
            .llm_preset_mapping
            .insert("context_compression".to_string(), "mapping".to_string());
        apply_role_mapping_fallbacks(&mut config);
        assert_eq!(
            config.context_compression.llm_preset.as_deref(),
            Some("explicit")
        );
    }
}
