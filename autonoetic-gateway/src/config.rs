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
        let derived_soft_budget = config.apply_profile_defaults();
        if let Some(value) = derived_soft_budget {
            tracing::info!(
                target: "autonoetic::prompt_budget",
                derived_soft_budget_tokens = value,
                "Derived proactive soft_budget_tokens from a large configured \
                 context window (#842) — context governor will now fire at the \
                 soft budget instead of waiting for the hard limit. Set \
                 prompt_budget.soft_budget_tokens explicitly to override.",
            );
        }
        apply_prompt_budget_overrides(&config);
        apply_llm_request_timeout(&config);
        Ok(config)
    } else {
        tracing::warn!("Config not found at {}, using defaults", path.display());
        let config = GatewayConfig::default();
        apply_prompt_budget_overrides(&config);
        apply_llm_request_timeout(&config);
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

/// Publish `llm_request_timeout_secs` to the LLM layer.
///
/// Applied here rather than at each CLI entry point so no command can start the
/// runtime with the config's timeout unread.
fn apply_llm_request_timeout(config: &GatewayConfig) {
    let Some(secs) = config.llm_request_timeout_secs else {
        return;
    };
    if secs < 5 {
        tracing::warn!(
            target: "autonoetic::llm",
            requested = secs,
            "llm_request_timeout_secs below the 5s floor; using default ({}s)",
            crate::llm::DEFAULT_REQUEST_TIMEOUT_SECS
        );
        return;
    }
    crate::llm::set_configured_request_timeout_secs(Some(secs));
    tracing::info!(
        target: "autonoetic::llm",
        applied_secs = secs,
        "Configured per-request LLM timeout applied"
    );
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

    /// `GatewayConfig` is `deny_unknown_fields`, so a config naming a key the
    /// binary doesn't know fails to load and the gateway won't start. This pins
    /// that an operator can write `llm_request_timeout_secs` and be parsed.
    #[test]
    fn llm_request_timeout_secs_is_an_accepted_config_key() {
        let cfg: GatewayConfig =
            serde_yaml::from_str("agents_dir: /tmp/agents\nllm_request_timeout_secs: 600\n")
                .expect("config with llm_request_timeout_secs must parse");
        assert_eq!(cfg.llm_request_timeout_secs, Some(600));
    }

    /// Setting the key twice is rejected, not silently resolved to one of the
    /// values. Pinned because it is an easy edit to make — raise the timeout by
    /// adding a line, leave the old one behind — and the failure lands on config
    /// load, which takes out the gateway *and* every CLI command with it. A
    /// future "last wins" would be worse than the error: the file would read one
    /// way and behave another.
    #[test]
    fn duplicating_llm_request_timeout_secs_is_rejected() {
        let err = serde_yaml::from_str::<GatewayConfig>(
            "agents_dir: /tmp/agents\nllm_request_timeout_secs: 600\nllm_request_timeout_secs: 300\n",
        )
        .expect_err("a duplicated key must not parse");
        assert!(
            err.to_string().contains("duplicate"),
            "error should name the duplication, got: {err}"
        );
    }

    /// Omitting it must stay valid — the field is an override, not a requirement.
    #[test]
    fn llm_request_timeout_secs_is_optional() {
        let cfg: GatewayConfig =
            serde_yaml::from_str("agents_dir: /tmp/agents\n").expect("config must parse");
        assert_eq!(cfg.llm_request_timeout_secs, None);
    }

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

    /// The uncommented (active) values in `config/config-template.yaml` must
    /// match `GatewayConfig::default()` — the template is documentation of the
    /// defaults, not a second source of truth. If this test fails, either the
    /// template drifted from the code or a default changed without the template
    /// being updated; fix the mismatch, don't relax the assertion.
    ///
    /// Excluded by design: `llm_presets` and `llm_preset_mapping` are
    /// documented *examples* (concrete provider/model names the operator must
    /// replace), not defaults — the built-in default is an empty map.
    #[test]
    fn config_template_uncommented_values_match_builtin_defaults() {
        let template = include_str!("../../config/config-template.yaml");
        let template_value: serde_yaml::Value = serde_yaml::from_str(template)
            .expect("config-template.yaml must be parseable YAML");
        let defaults_value = serde_yaml::to_value(GatewayConfig::default())
            .expect("GatewayConfig::default() must serialize");

        let template_map = template_value.as_mapping().expect("template must be a map");
        let defaults_map = defaults_value.as_mapping().expect("defaults must be a map");

        let mut mismatches: Vec<String> = Vec::new();
        for (key, template_val) in template_map {
            let key_str = key.as_str().unwrap_or_default().to_string();
            // Documented examples — the operator replaces these; not defaults.
            if matches!(key_str.as_str(), "llm_presets" | "llm_preset_mapping") {
                continue;
            }
            match defaults_map.get(key) {
                None => mismatches.push(format!(
                    "template key `{key_str}` has no built-in default (unknown field?)"
                )),
                Some(default_val) => {
                    collect_value_mismatches(
                        &format!("{key_str}"),
                        default_val,
                        template_val,
                        &mut mismatches,
                    );
                }
            }
        }

        assert!(
            mismatches.is_empty(),
            "config-template.yaml drifted from GatewayConfig::default() — update the template to match the code:\n  {}",
            mismatches.join("\n  ")
        );
    }

    /// Recursively compare a defaults subtree against the template's uncommented
    /// values, recording every differing leaf path.
    fn collect_value_mismatches(
        path: &str,
        default_val: &serde_yaml::Value,
        template_val: &serde_yaml::Value,
        out: &mut Vec<String>,
    ) {
        match (default_val, template_val) {
            (serde_yaml::Value::Mapping(dm), serde_yaml::Value::Mapping(tm)) => {
                for (tkey, tval) in tm {
                    let tkey_str = tkey.as_str().unwrap_or_default();
                    match dm.get(tkey) {
                        None => out.push(format!(
                            "{path}.{tkey_str}: in template but not in built-in defaults"
                        )),
                        Some(dval) => {
                            collect_value_mismatches(
                                &format!("{path}.{tkey_str}"),
                                dval,
                                tval,
                                out,
                            );
                        }
                    }
                }
            }
            (d, t) => {
                if d != t {
                    out.push(format!("{path}: template={t:?} default={d:?}"));
                }
            }
        }
    }
}
