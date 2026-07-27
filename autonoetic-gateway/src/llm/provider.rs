//! Provider resolution — maps a raw `LlmConfig` into a concrete, resolved
//! endpoint + auth configuration.
//!
//! Drivers should never read environment variables directly; they receive a
//! `ResolvedProvider` already populated by this module.

use autonoetic_types::egress::EgressClass;

/// How an OpenAI-compatible provider expects reasoning/thinking to be requested.
/// Drivers use this to pick the right request-body shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningStyle {
    /// Provider exposes no reasoning controls — the `thinking` field is dropped.
    None,
    /// OpenAI o-series / GPT-5: top-level string `"reasoning_effort": "low|medium|high"`.
    /// The driver must additionally gate emission by model name, because OpenAI
    /// rejects this field on non-reasoning models.
    OpenAiEffort,
    /// OpenRouter unified reasoning API: top-level object
    /// `"reasoning": {"effort": "low|medium|high", "max_tokens": N, ...}`.
    /// OpenRouter silently ignores the field on models that don't support
    /// reasoning, so the driver emits it unconditionally whenever `thinking` is set.
    OpenRouterUnified,
    /// OpenCode Go gateway (opencode.ai/zen/go). Supports Anthropic-style
    /// `cache_control` breakpoints on all models except GLM/Zhipu, plus
    /// per-session `prompt_cache_key` and `prompt_cache_retention: "24h"`.
    OpenCodeGo,
}

/// Flags describing what a provider's API supports.
/// Drivers use these to decide which code paths to take.
#[derive(Debug, Clone)]
pub struct ProviderCapabilities {
    /// Provider supports real SSE streaming (not just simulated).
    pub supports_streaming: bool,
    /// Provider streams individual tool-input JSON deltas during streaming.
    pub supports_tool_stream_deltas: bool,
    /// Provider requires system prompt at top level (not in the messages array).
    pub supports_system_top_level: bool,
    /// Provider includes token usage counts in stream chunks.
    pub supports_usage_in_stream: bool,
    /// Provider supports tool/function calling at all.
    pub supports_tools: bool,
    /// Provider supports the tool_choice parameter (some OpenAI-compatible APIs don't).
    pub supports_tool_choice: bool,
    /// Provider rejects JSON Schema where `type` sits alongside `anyOf`/`oneOf` at the
    /// same level; the `type` must be moved into each branch item instead.
    pub strict_schema_anyof: bool,
    /// Which reasoning-request schema the provider expects (or `None`).
    pub reasoning: ReasoningStyle,
}

impl ProviderCapabilities {
    /// OpenAI-compatible endpoints (OpenAI, OpenRouter, Groq, etc.)
    /// Reasoning defaults to `None`; per-provider entries override as needed.
    pub fn openai_compatible() -> Self {
        Self {
            supports_streaming: true,
            supports_tool_stream_deltas: true,
            supports_system_top_level: false,
            supports_usage_in_stream: false,
            supports_tools: true,
            supports_tool_choice: true,
            strict_schema_anyof: false,
            reasoning: ReasoningStyle::None,
        }
    }

    /// Basic chat-only provider (Z.AI GLM, some Chinese models via OpenRouter)
    pub fn chat_only() -> Self {
        Self {
            supports_streaming: true,
            supports_tool_stream_deltas: false,
            supports_system_top_level: false,
            supports_usage_in_stream: false,
            supports_tools: false,
            supports_tool_choice: false,
            strict_schema_anyof: false,
            reasoning: ReasoningStyle::None,
        }
    }

    /// Anthropic Messages API — handled by the dedicated Anthropic driver,
    /// so the `reasoning` field on capabilities is unused.
    pub fn anthropic() -> Self {
        Self {
            supports_streaming: true,
            supports_tool_stream_deltas: true,
            supports_system_top_level: true,
            supports_usage_in_stream: true,
            supports_tools: true,
            supports_tool_choice: true,
            strict_schema_anyof: false,
            reasoning: ReasoningStyle::None,
        }
    }

    /// Google Gemini generateContent API — handled by the dedicated Gemini
    /// driver, so the `reasoning` field on capabilities is unused.
    pub fn gemini() -> Self {
        Self {
            supports_streaming: false, // we don't implement Gemini streaming yet
            supports_tool_stream_deltas: false,
            supports_system_top_level: true,
            supports_usage_in_stream: false,
            supports_tools: true,
            supports_tool_choice: false,
            strict_schema_anyof: false,
            reasoning: ReasoningStyle::None,
        }
    }
}

/// Which authentication strategy to use.
#[derive(Debug, Clone)]
pub enum AuthStrategy {
    /// `Authorization: Bearer <key>` header (OpenAI-style)
    BearerToken(String),
    /// `x-api-key: <key>` header (Anthropic-style)
    XApiKey(String),
    /// `x-goog-api-key: <key>` header (Gemini-style)
    GoogleApiKey(String),
    /// No authentication required (e.g., local Ollama)
    None,
}

/// The family of wire protocol that a driver should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverKind {
    OpenAi,
    Anthropic,
    Gemini,
}

/// Fully-resolved provider configuration ready for use by a driver.
/// No driver should ever call `std::env::var` directly.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub kind: DriverKind,
    pub base_url: String,
    pub model: String,
    pub auth: AuthStrategy,
    pub capabilities: ProviderCapabilities,
    /// Extra HTTP headers to attach (e.g., OpenRouter attribution headers)
    pub extra_headers: Vec<(String, String)>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Egress classification of this resolved endpoint — RFC data-envelopes
    /// §5.1. `Local` for ollama/vllm/lmstudio/llamacpp (or explicit override),
    /// `Remote` otherwise (fail-closed default). The chokepoint (phase 1b #905)
    /// maps this to a [`autonoetic_types::egress::Sink`] to filter outbound
    /// content.
    pub egress_class: EgressClass,
}

/// Map a provider name to the reasoning-request schema its API expects.
/// Unknown providers default to `None` (silent drop), matching the
/// pre-change behavior for everything except OpenAI o-series.
fn reasoning_style_for_provider(provider: &str) -> ReasoningStyle {
    match provider {
        "openai" | "codex" => ReasoningStyle::OpenAiEffort,
        "openrouter" => ReasoningStyle::OpenRouterUnified,
        "opencode" => ReasoningStyle::OpenCodeGo,
        _ => ReasoningStyle::None,
    }
}

// ---------------------------------------------------------------------------
// Known provider defaults table
// ---------------------------------------------------------------------------

struct ProviderDefaults {
    base_url: &'static str,
    api_key_env: &'static str,
    kind: DriverKind,
    capabilities: fn() -> ProviderCapabilities,
    /// If set, this provider/model requires this exact temperature and ignores
    /// the preset/agent-level temperature (e.g. Kimi Code only accepts 1.0).
    fixed_temperature: Option<f32>,
    /// Inferred egress classification of this provider (RFC data-envelopes
    /// §5.1): `Local` for ollama/vllm/lmstudio/llamacpp, `Remote` otherwise.
    /// Overridable from the preset's `egress_class` — a remote Ollama server is
    /// a real deployment shape.
    egress_class: EgressClass,
}

fn provider_defaults(name: &str) -> Option<ProviderDefaults> {
    match name {
        "anthropic" | "claude" => Some(ProviderDefaults {
            base_url: "https://api.anthropic.com/v1/messages",
            api_key_env: "ANTHROPIC_API_KEY",
            kind: DriverKind::Anthropic,
            capabilities: ProviderCapabilities::anthropic,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "gemini" | "google" => Some(ProviderDefaults {
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            api_key_env: "GEMINI_API_KEY",
            kind: DriverKind::Gemini,
            capabilities: ProviderCapabilities::gemini,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        // ----------- OpenAI-compatible providers (single code path) -----------
        "openai" | "codex" => Some(ProviderDefaults {
            base_url: "https://api.openai.com/v1/chat/completions",
            api_key_env: "OPENAI_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "opencode" => Some(ProviderDefaults {
            base_url: "https://opencode.ai/zen/go/v1/chat/completions",
            api_key_env: "OPENCODE_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "openrouter" => Some(ProviderDefaults {
            base_url: "https://openrouter.ai/api/v1/chat/completions",
            api_key_env: "OPENROUTER_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "groq" => Some(ProviderDefaults {
            base_url: "https://api.groq.com/openai/v1/chat/completions",
            api_key_env: "GROQ_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "together" => Some(ProviderDefaults {
            base_url: "https://api.together.xyz/v1/chat/completions",
            api_key_env: "TOGETHER_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "deepseek" => Some(ProviderDefaults {
            base_url: "https://api.deepseek.com/v1/chat/completions",
            api_key_env: "DEEPSEEK_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "mistral" => Some(ProviderDefaults {
            base_url: "https://api.mistral.ai/v1/chat/completions",
            api_key_env: "MISTRAL_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "fireworks" => Some(ProviderDefaults {
            base_url: "https://api.fireworks.ai/inference/v1/chat/completions",
            api_key_env: "FIREWORKS_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "perplexity" => Some(ProviderDefaults {
            base_url: "https://api.perplexity.ai/chat/completions",
            api_key_env: "PERPLEXITY_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "cohere" => Some(ProviderDefaults {
            base_url: "https://api.cohere.com/compatibility/v1/chat/completions",
            api_key_env: "COHERE_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "ai21" => Some(ProviderDefaults {
            base_url: "https://api.ai21.com/studio/v1/chat/completions",
            api_key_env: "AI21_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "cerebras" => Some(ProviderDefaults {
            base_url: "https://api.cerebras.ai/v1/chat/completions",
            api_key_env: "CEREBRAS_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "sambanova" => Some(ProviderDefaults {
            base_url: "https://api.sambanova.ai/v1/chat/completions",
            api_key_env: "SAMBANOVA_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "huggingface" => Some(ProviderDefaults {
            base_url: "https://api-inference.huggingface.co/v1/chat/completions",
            api_key_env: "HUGGINGFACE_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "xai" => Some(ProviderDefaults {
            base_url: "https://api.x.ai/v1/chat/completions",
            api_key_env: "XAI_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "replicate" => Some(ProviderDefaults {
            base_url: "https://api.replicate.com/v1/deployments",
            api_key_env: "REPLICATE_API_TOKEN",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "moonshot" | "kimi" => Some(ProviderDefaults {
            base_url: "https://api.moonshot.cn/v1/chat/completions",
            api_key_env: "MOONSHOT_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        "kimi-code" => Some(ProviderDefaults {
            base_url: "https://api.kimi.com/coding/v1/chat/completions",
            api_key_env: "KIMI_CODE_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: Some(1.0),
            egress_class: EgressClass::Remote,
        }),
        "qwen" | "dashscope" => Some(ProviderDefaults {
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
            api_key_env: "DASHSCOPE_API_KEY",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Remote,
        }),
        // Local providers — no API key needed
        "ollama" => Some(ProviderDefaults {
            base_url: "http://localhost:11434/v1/chat/completions",
            api_key_env: "",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Local,
        }),
        "vllm" => Some(ProviderDefaults {
            base_url: "http://localhost:8000/v1/chat/completions",
            api_key_env: "",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Local,
        }),
        "lmstudio" => Some(ProviderDefaults {
            base_url: "http://localhost:1234/v1/chat/completions",
            api_key_env: "",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Local,
        }),
        "llamacpp" | "llama.cpp" => Some(ProviderDefaults {
            base_url: "http://localhost:8080/v1/chat/completions",
            api_key_env: "",
            kind: DriverKind::OpenAi,
            capabilities: ProviderCapabilities::openai_compatible,
            fixed_temperature: None,
            egress_class: EgressClass::Local,
        }),
        _ => None,
    }
}

/// Resolve a provider name + optional overrides into a `ResolvedProvider`.
/// Returns an error if an API key is required but missing from the environment.
///
/// `egress_class_override` (RFC data-envelopes §5.1): when set, takes
/// precedence over the inferred class — the preset's explicit classification
/// wins. Otherwise the class is inferred from [`provider_defaults`] (local for
/// ollama/vllm/lmstudio/llamacpp) and falls back to `Remote` for unknown
/// providers (fail-closed, RFC §2.2).
pub fn resolve(
    provider: &str,
    model: &str,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    base_url_override: Option<&str>,
    api_key_override: Option<&str>,
    chat_only: bool,
    egress_class_override: Option<EgressClass>,
) -> anyhow::Result<ResolvedProvider> {
    let defaults = provider_defaults(provider);

    let (kind, base_url, mut capabilities, fixed_temperature, inferred_egress_class) =
        if let Some(ref d) = defaults {
            (
                d.kind.clone(),
                base_url_override.unwrap_or(d.base_url).to_string(),
                (d.capabilities)(),
                d.fixed_temperature,
                d.egress_class,
            )
        } else if let Some(url) = base_url_override {
            // Unknown provider with a custom URL — treat as OpenAI-compatible
            // and remote (fail-closed: no inference possible).
            (
                DriverKind::OpenAi,
                url.to_string(),
                ProviderCapabilities::openai_compatible(),
                None,
                EgressClass::Remote,
            )
        } else {
            anyhow::bail!(
                "Unknown provider '{}' and no base_url override provided",
                provider
            )
        };

    // Explicit preset classification wins over inference.
    let egress_class = egress_class_override.unwrap_or(inferred_egress_class);

    // Override to chat-only mode if explicitly set
    if chat_only {
        capabilities = ProviderCapabilities::chat_only();
    }

    // Per-provider reasoning style (only applies to OpenAI-shape providers;
    // Anthropic and Gemini drivers use their own native reasoning paths).
    capabilities.reasoning = reasoning_style_for_provider(provider);

    // Providers such as Kimi Code only accept a single fixed temperature.
    let temperature = fixed_temperature.or(temperature);

    // Moonshot's schema validator requires `type` to live inside each anyOf/oneOf
    // branch instead of at the parent level.
    capabilities.strict_schema_anyof = matches!(
        provider,
        "moonshot" | "kimi" | "kimi-code"
    );

    // Resolve auth
    let api_key = if let Some(k) = api_key_override {
        k.to_string()
    } else if let Some(ref d) = defaults {
        if d.api_key_env.is_empty() {
            String::new() // no key needed (Ollama)
        } else {
            std::env::var(d.api_key_env).map_err(|_| {
                anyhow::anyhow!(
                    "Missing {} environment variable for provider '{}'",
                    d.api_key_env,
                    provider
                )
            })?
        }
    } else {
        String::new()
    };

    let auth = match kind {
        DriverKind::Anthropic => AuthStrategy::XApiKey(api_key),
        DriverKind::Gemini => AuthStrategy::GoogleApiKey(api_key),
        DriverKind::OpenAi => {
            if api_key.is_empty() {
                AuthStrategy::None
            } else {
                AuthStrategy::BearerToken(api_key)
            }
        }
    };

    // Attach OpenRouter attribution headers
    let extra_headers = if provider == "openrouter" {
        vec![
            (
                "HTTP-Referer".to_string(),
                "https://autonoetic.local".to_string(),
            ),
            (
                "X-OpenRouter-Title".to_string(),
                "Autonoetic Gateway".to_string(),
            ),
        ]
    } else {
        vec![]
    };

    Ok(ResolvedProvider {
        kind,
        base_url,
        model: model.to_string(),
        auth,
        capabilities,
        extra_headers,
        temperature,
        max_tokens,
        egress_class,
    })
}

#[cfg(test)]
mod tests {
    //! Provider egress classification — RFC data-envelopes §5.1.
    //!
    //! The chokepoint (phase 1b #905) maps `ResolvedProvider.egress_class` to a
    //! [`autonoetic_types::egress::Sink`]; these tests pin the inference + the
    //! override precedence that the chokepoint relies on.
    use super::*;

    /// Helper: resolve without any API key in the env, swallowing the auth
    /// error when the provider demands one. Returns the `egress_class` that
    /// *classification* would assign (independent of auth resolution).
    fn classify(provider: &str, override_class: Option<EgressClass>) -> EgressClass {
        // `resolve` may fail on missing API key for providers that need one;
        // classification itself does not depend on the key, so fall back to the
        // inferred `provider_defaults` class on auth error.
        match resolve(
            provider,
            "test-model",
            None,
            None,
            None,
            Some("dummy-key-not-sent"), // avoid env-var lookup failures
            false,
            override_class,
        ) {
            Ok(r) => r.egress_class,
            Err(_) => provider_defaults(provider)
                .map(|d| d.egress_class)
                .unwrap_or(EgressClass::Remote),
        }
    }

    #[test]
    fn local_providers_are_inferred_local() {
        for p in ["ollama", "vllm", "lmstudio", "llamacpp", "llama.cpp"] {
            assert_eq!(
                classify(p, None),
                EgressClass::Local,
                "{p} should infer local"
            );
        }
    }

    #[test]
    fn remote_providers_are_inferred_remote() {
        for p in [
            "anthropic",
            "claude",
            "openai",
            "openrouter",
            "gemini",
            "deepseek",
            "mistral",
            "groq",
            "together",
            "xai",
        ] {
            assert_eq!(
                classify(p, None),
                EgressClass::Remote,
                "{p} should infer remote"
            );
        }
    }

    #[test]
    fn unknown_provider_is_remote_fail_closed() {
        // No defaults entry → inferred Remote. (We bypass `resolve`'s unknown-
        // provider bail by reading the table directly, mirroring the fail-closed
        // semantics the chokepoint depends on.)
        let inferred = provider_defaults("totally-unknown-provider")
            .map(|d| d.egress_class)
            .unwrap_or(EgressClass::Remote);
        assert_eq!(inferred, EgressClass::Remote);
    }

    #[test]
    fn explicit_override_wins_over_inference() {
        // ollama infers Local; an explicit `remote` override (a remote Ollama
        // server is a real deployment shape) must win.
        assert_eq!(
            classify("ollama", Some(EgressClass::Remote)),
            EgressClass::Remote
        );
        // anthropic infers Remote; an explicit `local` override wins.
        assert_eq!(
            classify("anthropic", Some(EgressClass::Local)),
            EgressClass::Local
        );
    }

    #[test]
    fn override_none_falls_back_to_inference() {
        assert_eq!(classify("ollama", None), EgressClass::Local);
        assert_eq!(classify("anthropic", None), EgressClass::Remote);
    }
}
