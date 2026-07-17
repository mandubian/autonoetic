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
        config.apply_profile_defaults();
        apply_prompt_budget_overrides(&config);
        Ok(config)
    } else {
        tracing::warn!("Config not found at {}, using defaults", path.display());
        let config = GatewayConfig::default();
        apply_prompt_budget_overrides(&config);
        Ok(config)
    }
}

/// Persist a `GatewayConfig` to a YAML file (used for operator-local preset injection).
///
/// Rewrites the file from structured data — inline comments and formatting are not preserved.
pub fn save_config(path: &Path, config: &GatewayConfig) -> anyhow::Result<()> {
    let yaml = serde_yaml::to_string(config)
        .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;
    let body = format!(
        "# WARNING: Rewritten by autonoetic — comments and manual formatting were not preserved.\n\
         # Re-merge from config/config-template.yaml or your backup before editing further.\n\n{yaml}"
    );
    std::fs::write(path, body)?;
    Ok(())
}

/// Push the configured `chars_per_token` override (if any) into the
/// process-wide atomic that the prompt-budget estimator reads. Called from
/// `load_config` so every code path that consumes a `GatewayConfig` ends up
/// with a consistent estimator calibration before the first LLM call.
fn apply_prompt_budget_overrides(config: &GatewayConfig) {
    if let Some(cpt) = config.prompt_budget.chars_per_token {
        if cpt.is_finite() && cpt > 0.0 {
            let stored = crate::runtime::prompt_budget::set_chars_per_token(cpt);
            tracing::info!(
                target: "autonoetic::prompt_budget",
                requested = cpt,
                applied = stored,
                "Configured chars_per_token override applied"
            );
        } else {
            // Malformed: log a warning and leave the default in place.
            tracing::warn!(
                target: "autonoetic::prompt_budget",
                requested = cpt,
                "Configured chars_per_token is not a positive finite number; using default ({})",
                crate::runtime::prompt_budget::DEFAULT_CHARS_PER_TOKEN
            );
        }
    }
}

/// Populate role-keyed config fields from `llm_preset_mapping` when they
/// are not set explicitly. This lets operators centralize preset choice
/// for cross-cutting roles (e.g. `context_compression`) alongside the
/// per-agent role mappings.
///
/// Only fires when no compression LLM is configured at all — explicit
/// `llm_preset`, explicit `provider`+`model`, and an agent-level override
/// all take precedence. `resolve_compression_llm_config` prefers
/// `llm_preset` over inline `provider`/`model`, so populating it
/// unconditionally would shadow inline configuration.
fn apply_role_mapping_fallbacks(config: &mut GatewayConfig) {
    let cc = &config.context_compression;
    let already_configured = cc.llm_preset.is_some()
        || (cc.provider.is_some() && cc.model.is_some());
    if already_configured {
        return;
    }
    if let Some(preset) = config.llm_preset_mapping.get("context_compression") {
        config.context_compression.llm_preset = Some(preset.clone());
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

    #[test]
    fn role_mapping_fallback_does_not_shadow_inline_provider_model() {
        let mut config = GatewayConfig::default();
        config.context_compression.provider = Some("anthropic".to_string());
        config.context_compression.model = Some("claude-haiku-3".to_string());
        config
            .llm_preset_mapping
            .insert("context_compression".to_string(), "mapping".to_string());
        apply_role_mapping_fallbacks(&mut config);
        // llm_preset must stay None — otherwise resolve_compression_llm_config
        // would short-circuit on preset lookup and ignore the inline config.
        assert!(config.context_compression.llm_preset.is_none());
        assert_eq!(
            config.context_compression.provider.as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            config.context_compression.model.as_deref(),
            Some("claude-haiku-3")
        );
    }

    #[test]
    fn starter_profile_defaults_evidence_mode_errors() {
        let mut config = GatewayConfig::default();
        config.profile = autonoetic_types::config::Profile::Starter;
        config.apply_profile_defaults();
        assert_eq!(config.evidence_mode, "errors");
        assert!(!config.session_report.live_html_on_update);
    }

    #[test]
    fn starter_profile_does_not_override_non_default_evidence_mode() {
        let mut config = GatewayConfig::default();
        config.profile = autonoetic_types::config::Profile::Starter;
        config.evidence_mode = "off".to_string();
        config.apply_profile_defaults();
        assert_eq!(config.evidence_mode, "off");
    }
}
