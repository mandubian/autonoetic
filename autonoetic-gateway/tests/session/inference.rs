//! Integration tests for session inference bindings and preset resolution.

use autonoetic_gateway::runtime::inference_profile::{
    resolve_inference_profile, validate_inference_override, PresetSource,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{
    AgentIdentity, AgentManifest, ExecutionMode, LlmConfig, RuntimeDeclaration,
};
use autonoetic_types::capability::Capability;
use autonoetic_types::config::{GatewayConfig, LlmPreset};
use std::collections::HashMap;
use tempfile::tempdir;
use crate::support::manifest_builder::TestManifest;

fn test_manifest() -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "coder.default".to_string(),
            name: "Coder".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: vec![Capability::SandboxFunctions {
            allowed: vec!["content.".to_string()],
        }],
        llm_preset: Some("sonnet".to_string()),
        execution_mode: ExecutionMode::Reasoning,
        ..TestManifest::new().build()
    }
}

fn test_config() -> GatewayConfig {
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
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    presets.insert(
        "fallback".to_string(),
        LlmPreset {
            provider: Some("openai".to_string()),
            model: Some("gpt-4o".to_string()),
            temperature: Some(0.2),
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
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    GatewayConfig {
        llm_presets: presets,
        ..Default::default()
    }
}

#[test]
fn session_inference_binding_roundtrip() {
    let dir = tempdir().unwrap();
    let store = GatewayStore::open(dir.path()).unwrap();
    let binding = store
        .upsert_session_inference_binding("root-1", Some("fallback"), Some("outage"), "operator:test")
        .unwrap();
    assert_eq!(binding.preset_override.as_deref(), Some("fallback"));

    let loaded = store.get_session_inference_binding("root-1").unwrap().unwrap();
    assert_eq!(loaded.preset_override.as_deref(), Some("fallback"));

    let profile = resolve_inference_profile(
        "coder.default",
        &test_manifest(),
        &test_config(),
        Some(&loaded),
    )
    .unwrap();
    assert_eq!(profile.preset_source, PresetSource::SessionOverride);
    assert_eq!(profile.llm_config.provider, "openai");
    assert_eq!(profile.llm_config.model, "gpt-4o");

    assert!(store.delete_session_inference_binding("root-1").unwrap());
    assert!(store.get_session_inference_binding("root-1").unwrap().is_none());
}

#[test]
fn session_inference_override_validates_chat_only() {
    let mut config = test_config();
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
            egress_class: None,
            request_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    let err = validate_inference_override(&test_manifest(), &config, "chat").unwrap_err();
    assert!(err.to_string().contains("chat_only"));
}
