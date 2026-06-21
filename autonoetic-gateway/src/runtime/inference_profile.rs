//! Resolve agent inference profiles from preset names, mapping, and session overrides.
//!
//! See `docs/rfc/llm-preset-inference-profiles.md`.

use autonoetic_types::agent::{AgentManifest, ExecutionMode, LlmConfig, LlmOverrides};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::GatewayConfig;
use serde::{Deserialize, Serialize};

use super::llm_preset_resolver::{
    is_routing_preset, resolve_fixed_preset, resolve_model_list, resolve_preset_name_for_agent,
};

/// Where the active preset name was resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetSource {
    SessionOverride,
    AgentManifest,
    Mapping,
    LegacyInline,
}

impl PresetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionOverride => "session_override",
            Self::AgentManifest => "agent_manifest",
            Self::Mapping => "mapping",
            Self::LegacyInline => "legacy_inline",
        }
    }
}

/// Persisted session-level operator override (see `session_inference_bindings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInferenceBinding {
    pub root_session_id: String,
    pub preset_override: Option<String>,
    pub reason: Option<String>,
    pub set_by: String,
    pub set_at: String,
}

/// Fully resolved inference profile for one agent invocation.
#[derive(Debug, Clone)]
pub struct ResolvedInferenceProfile {
    pub preset_name: Option<String>,
    pub preset_source: PresetSource,
    pub session_override_preset: Option<String>,
    pub llm_config: LlmConfig,
    pub is_routing_preset: bool,
}

impl ResolvedInferenceProfile {
    pub fn snapshot_preset_source(&self) -> String {
        self.preset_source.as_str().to_string()
    }
}

/// Resolve the inference profile for an agent session.
pub fn resolve_inference_profile(
    agent_id: &str,
    manifest: &AgentManifest,
    config: &GatewayConfig,
    binding: Option<&SessionInferenceBinding>,
) -> anyhow::Result<ResolvedInferenceProfile> {
    let session_override = binding.and_then(|b| b.preset_override.clone());

    let (preset_name, preset_source) = if let Some(ref name) = session_override {
        (Some(name.clone()), PresetSource::SessionOverride)
    } else if let Some(ref name) = manifest.llm_preset {
        let remapped = config
            .llm_preset_mapping
            .get(name.as_str())
            .cloned();
        (Some(remapped.unwrap_or_else(|| name.clone())), PresetSource::AgentManifest)
    } else if let Some(name) = resolve_preset_name_for_agent(agent_id, &config.llm_preset_mapping) {
        (Some(name.to_string()), PresetSource::Mapping)
    } else if manifest.llm_config.is_some() {
        return Ok(legacy_inline_profile(manifest));
    } else {
        anyhow::bail!(
            "Agent '{}' has no llm_preset, mapping entry, or legacy llm_config",
            agent_id
        );
    };

    let preset_name = preset_name.expect("preset_name set when not legacy");
    let preset = config
        .llm_presets
        .get(&preset_name)
        .ok_or_else(|| anyhow::anyhow!("Unknown llm preset '{}'", preset_name))?;

    let manifest_fallback = manifest.llm_config.as_ref();
    let is_routing = is_routing_preset(preset);

    let llm_config = if is_routing {
        let routing = preset.routing.as_ref().expect("routing preset");
        let models = resolve_model_list(routing, &config.llm_presets);
        let base = models
            .first()
            .map(|e| e.config.clone())
            .or_else(|| manifest_fallback.cloned())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Routing preset '{}' has no resolvable models and agent has no llm_config fallback",
                    preset_name
                )
            })?;
        let mut cfg = base;
        cfg.routing_preset = Some(preset_name.clone());
        merge_manifest_llm_hints(&mut cfg, manifest_fallback);
        apply_llm_overrides(&mut cfg, manifest.llm_overrides.as_ref());
        cfg
    } else {
        let mut cfg = resolve_fixed_preset(preset).ok_or_else(|| {
            anyhow::anyhow!("Preset '{}' is not a fixed provider/model preset", preset_name)
        })?;
        merge_manifest_llm_hints(&mut cfg, manifest_fallback);
        apply_llm_overrides(&mut cfg, manifest.llm_overrides.as_ref());
        cfg
    };

    Ok(ResolvedInferenceProfile {
        preset_name: Some(preset_name),
        preset_source,
        session_override_preset: session_override,
        llm_config,
        is_routing_preset: is_routing,
    })
}

fn legacy_inline_profile(manifest: &AgentManifest) -> ResolvedInferenceProfile {
    let llm_config = manifest.llm_config.clone().expect("legacy llm_config");
    ResolvedInferenceProfile {
        preset_name: llm_config.routing_preset.clone(),
        preset_source: PresetSource::LegacyInline,
        session_override_preset: None,
        llm_config,
        is_routing_preset: manifest
            .llm_config
            .as_ref()
            .and_then(|c| c.routing_preset.as_ref())
            .and_then(|name| {
                // Best-effort: routing if routing_preset field set on legacy config.
                Some(!name.is_empty())
            })
            .unwrap_or(false),
    }
}

fn apply_llm_overrides(cfg: &mut LlmConfig, overrides: Option<&LlmOverrides>) {
    let Some(o) = overrides else {
        return;
    };
    if let Some(temp) = o.temperature {
        cfg.temperature = temp;
    }
    if o.thinking.is_some() {
        cfg.thinking = o.thinking.clone();
    }
    if let Some(tokens) = o.context_window_tokens {
        cfg.context_window_tokens = Some(tokens);
    }
}

fn merge_manifest_llm_hints(cfg: &mut LlmConfig, manifest: Option<&LlmConfig>) {
    let Some(m) = manifest else {
        return;
    };
    if m.thinking.is_some() {
        cfg.thinking = m.thinking.clone();
    }
    // Preserve manifest temperature when it differs from the preset default (0.2),
    // including explicit 0.0 (fully deterministic).
    const PRESET_DEFAULT_TEMPERATURE: f64 = 0.2;
    if (cfg.temperature - PRESET_DEFAULT_TEMPERATURE).abs() < f64::EPSILON
        && (m.temperature - cfg.temperature).abs() > f64::EPSILON
    {
        cfg.temperature = m.temperature;
    }
}

/// Returns true when the agent needs tool-capable models (not chat-only).
pub fn agent_requires_tool_capable_llm(manifest: &AgentManifest) -> bool {
    if manifest.execution_mode == ExecutionMode::Script {
        return false;
    }
    manifest.capabilities.iter().any(|c| {
        matches!(
            c,
            Capability::SandboxFunctions { .. }
                | Capability::AgentSpawn { .. }
                | Capability::CodeExecution { .. }
        )
    }) || !manifest.allowed_tool_tiers.is_empty()
}

/// Validate a session override preset before persisting.
pub fn validate_inference_override(
    manifest: &AgentManifest,
    config: &GatewayConfig,
    preset_name: &str,
) -> anyhow::Result<LlmConfig> {
    let preset = config
        .llm_presets
        .get(preset_name)
        .ok_or_else(|| anyhow::anyhow!("Unknown llm preset '{}'", preset_name))?;

    let preview = if is_routing_preset(preset) {
        let routing = preset.routing.as_ref().expect("routing preset");
        let models = resolve_model_list(routing, &config.llm_presets);
        models
            .first()
            .map(|e| e.config.clone())
            .or_else(|| manifest.llm_config.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Routing preset '{}' has no models to validate against",
                    preset_name
                )
            })?
    } else {
        resolve_fixed_preset(preset).ok_or_else(|| {
            anyhow::anyhow!("Preset '{}' is not a fixed provider/model preset", preset_name)
        })?
    };

    if agent_requires_tool_capable_llm(manifest) && preview.chat_only {
        anyhow::bail!(
            "Preset '{}' is chat_only but agent '{}' requires tool-capable models",
            preset_name,
            manifest.agent.id
        );
    }

    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::agent::{AgentIdentity, RuntimeDeclaration};
    use autonoetic_types::config::LlmPreset;
    use std::collections::HashMap;

    fn test_manifest() -> AgentManifest {
        AgentManifest {
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
                id: "coder.default".to_string(),
                name: "Coder".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![Capability::SandboxFunctions {
                allowed: vec!["content.".to_string()],
            }],
            llm_preset: None,
            llm_overrides: None,
            llm_config: None,
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
            agentskills_import: None,
            compression: None,
            open_web: false,
            sandbox_network: Default::default(),
        }
    }

    fn fixed_config() -> GatewayConfig {
        let mut presets = HashMap::new();
        presets.insert(
            "sonnet".to_string(),
            LlmPreset {
                provider: Some("anthropic".to_string()),
                model: Some("claude-sonnet-4-20250514".to_string()),
                temperature: Some(0.1),
                fallback_provider: None,
                fallback_model: None,
                chat_only: Some(false),
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: None,
                cost: None,
                latency: None,
                routing: None,
            },
        );
        let mut mapping = HashMap::new();
        mapping.insert("coder.default".to_string(), "sonnet".to_string());
        GatewayConfig {
            llm_presets: presets,
            llm_preset_mapping: mapping,
            ..Default::default()
        }
    }

    #[test]
    fn resolves_from_mapping() {
        let profile =
            resolve_inference_profile("coder.default", &test_manifest(), &fixed_config(), None)
                .unwrap();
        assert_eq!(profile.preset_source, PresetSource::Mapping);
        assert_eq!(profile.preset_name.as_deref(), Some("sonnet"));
        assert_eq!(profile.llm_config.provider, "anthropic");
        assert_eq!(profile.llm_config.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn session_override_wins() {
        let binding = SessionInferenceBinding {
            root_session_id: "s1".to_string(),
            preset_override: Some("sonnet".to_string()),
            reason: Some("outage".to_string()),
            set_by: "operator:cli".to_string(),
            set_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let profile = resolve_inference_profile(
            "coder.default",
            &test_manifest(),
            &fixed_config(),
            Some(&binding),
        )
        .unwrap();
        assert_eq!(profile.preset_source, PresetSource::SessionOverride);
    }

    #[test]
    fn legacy_inline_still_works() {
        let mut manifest = test_manifest();
        manifest.llm_config = Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.2,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
        });
        let profile = resolve_inference_profile(
            "legacy.unmapped.agent",
            &manifest,
            &fixed_config(),
            None,
        )
        .unwrap();
        assert_eq!(profile.preset_source, PresetSource::LegacyInline);
        assert_eq!(profile.llm_config.model, "gpt-4o");
    }

    #[test]
    fn llm_overrides_win_over_legacy_thinking_hints() {
        use autonoetic_types::agent::{ThinkingConfig, ThinkingEffort};

        let mut manifest = test_manifest();
        manifest.llm_preset = Some("sonnet".to_string());
        manifest.llm_config = Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.2,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: Some(ThinkingConfig {
                effort: ThinkingEffort::Low,
                budget_tokens: None,
            }),
        });
        manifest.llm_overrides = Some(LlmOverrides {
            temperature: None,
            thinking: Some(ThinkingConfig {
                effort: ThinkingEffort::High,
                budget_tokens: None,
            }),
            context_window_tokens: None,
        });
        let profile =
            resolve_inference_profile("coder.default", &manifest, &fixed_config(), None).unwrap();
        assert_eq!(
            profile.llm_config.thinking.as_ref().map(|t| t.effort),
            Some(ThinkingEffort::High)
        );
    }

    #[test]
    fn llm_overrides_apply_temperature() {
        let mut manifest = test_manifest();
        manifest.llm_preset = Some("sonnet".to_string());
        manifest.llm_overrides = Some(LlmOverrides {
            temperature: Some(0.0),
            thinking: None,
            context_window_tokens: None,
        });
        let profile =
            resolve_inference_profile("coder.default", &manifest, &fixed_config(), None).unwrap();
        assert_eq!(profile.llm_config.temperature, 0.0);
    }

    #[test]
    fn merge_manifest_temperature_zero_when_preset_default() {
        let mut config = fixed_config();
        config.llm_presets.get_mut("sonnet").unwrap().temperature = None;
        let mut manifest = test_manifest();
        manifest.llm_config = Some(LlmConfig {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.0,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
        });
        manifest.llm_preset = Some("sonnet".to_string());
        let profile =
            resolve_inference_profile("coder.default", &manifest, &config, None).unwrap();
        assert_eq!(profile.llm_config.temperature, 0.0);
    }

    #[test]
    fn chat_only_override_rejected_for_tool_agent() {
        let mut config = fixed_config();
        config.llm_presets.insert(
            "chat".to_string(),
            LlmPreset {
                provider: Some("openai".to_string()),
                model: Some("gpt-4o".to_string()),
                temperature: Some(0.2),
                fallback_provider: None,
                fallback_model: None,
                chat_only: Some(true),
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                thinking: None,
                tier: None,
                cost: None,
                latency: None,
                routing: None,
            },
        );
        let err = validate_inference_override(&test_manifest(), &config, "chat").unwrap_err();
        assert!(err.to_string().contains("chat_only"));
    }
}
