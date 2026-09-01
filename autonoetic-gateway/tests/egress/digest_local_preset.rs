//! Post-session digest LLM preset selection under session egress taint (#947).
//!
//! Pins the security-relevant branch in `resolve_digest_llm_for_session`: a
//! session whose taint excludes RemoteModel must run the post-session digest
//! on a `egress_class: local` preset, and must refuse rather than ship
//! tainted trace content to a remote model when no local preset is configured.

use autonoetic_gateway::runtime::post_session_digest::resolve_digest_llm_for_session;
use autonoetic_types::config::{GatewayConfig, LlmPreset};
use autonoetic_types::egress::{EgressClass, EgressLabel};

fn local_preset(model: &str) -> LlmPreset {
    LlmPreset {
        provider: Some("ollama".to_string()),
        model: Some(model.to_string()),
        temperature: Some(0.0),
        fallback_provider: None,
        fallback_model: None,
        chat_only: Some(true),
        context_window_tokens: None,
        max_tokens: None,
        base_url: None,
        api_key_env: None,
        thinking: None,
        tier: None,
        cost: None,
        latency: None,
        routing: None,
        egress_class: Some(EgressClass::Local),
        request_timeout_secs: None,
        ttfb_timeout_secs: None,
    }
}

fn remote_preset(model: &str) -> LlmPreset {
    LlmPreset {
        egress_class: Some(EgressClass::Remote),
        ..local_preset(model)
    }
}

fn config_with_presets(digest_preset: &str, presets: &[(&str, LlmPreset)]) -> GatewayConfig {
    let mut config = GatewayConfig::default();
    config.digest_agent.llm_preset = Some(digest_preset.to_string());
    for (name, preset) in presets {
        config.llm_presets.insert(name.to_string(), preset.clone());
    }
    config
}

#[test]
fn tainted_session_forces_local_digest_preset() {
    // Configured digest preset is remote; session taint excludes RemoteModel
    // → digest must fall back to the (deterministically sorted) local preset.
    let config = config_with_presets(
        "remote-1",
        &[
            ("remote-1", remote_preset("remote-1-model")),
            ("local-b", local_preset("local-b-model")),
            ("local-a", local_preset("local-a-model")),
        ],
    );
    let cfg = resolve_digest_llm_for_session(&config, Some(&EgressLabel::local_only())).unwrap();
    assert_eq!(cfg.model, "local-a-model", "must pick first local preset in sorted order");
    assert_eq!(cfg.egress_class, Some(EgressClass::Local));
}

#[test]
fn tainted_session_keeps_already_local_digest_preset() {
    // Configured digest preset is already local → keep it, no fallback.
    let config = config_with_presets(
        "local-a",
        &[
            ("remote-1", remote_preset("remote-1-model")),
            ("local-a", local_preset("local-a-model")),
            ("local-b", local_preset("local-b-model")),
        ],
    );
    let cfg = resolve_digest_llm_for_session(&config, Some(&EgressLabel::local_only())).unwrap();
    assert_eq!(cfg.model, "local-a-model");
}

#[test]
fn tainted_session_without_local_preset_refuses_digest() {
    // No `egress_class: local` preset configured → refuse rather than ship
    // tainted trace content to a remote model.
    let config = config_with_presets("remote-1", &[("remote-1", remote_preset("remote-1-model"))]);
    let err =
        resolve_digest_llm_for_session(&config, Some(&EgressLabel::local_only())).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("refusing digest"), "unexpected error: {msg}");
    assert!(msg.contains("egress_class: local"), "unexpected error: {msg}");
}

#[test]
fn clean_session_uses_configured_digest_preset() {
    // No taint → unchanged behavior: the configured preset is used even
    // when it is remote and local presets exist.
    let config = config_with_presets(
        "remote-1",
        &[
            ("remote-1", remote_preset("remote-1-model")),
            ("local-a", local_preset("local-a-model")),
        ],
    );
    let cfg = resolve_digest_llm_for_session(&config, None).unwrap();
    assert_eq!(cfg.model, "remote-1-model");
    assert_eq!(cfg.egress_class, Some(EgressClass::Remote));
}

#[test]
fn unrestricted_taint_uses_configured_digest_preset() {
    // Taint that still allows RemoteModel → no forcing.
    let config = config_with_presets(
        "remote-1",
        &[
            ("remote-1", remote_preset("remote-1-model")),
            ("local-a", local_preset("local-a-model")),
        ],
    );
    let cfg = resolve_digest_llm_for_session(&config, Some(&EgressLabel::unrestricted())).unwrap();
    assert_eq!(cfg.model, "remote-1-model");
}
