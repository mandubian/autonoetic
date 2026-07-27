//! Smart model routing for LLM completions.
//!
//! Provides deterministic model selection based on budget pressure,
//! task complexity, and cost constraints. Falls back through a
//! configured chain on failure.

use crate::llm::{CompletionRequest, Message, Role};
use autonoetic_types::agent::LlmConfig;
use autonoetic_types::config::{
    ApprovalGatesConfig, CapabilityTier, DeterministicRoutingConfig, HybridRoutingConfig,
    LlmRoutingConfig, RoutingContext, RoutingPresetConfig, RoutingStrategy,
};
use serde::{Deserialize, Serialize};

/// A resolved model entry from a fixed preset, carrying all metadata
/// needed for routing decisions.
#[derive(Debug, Clone)]
pub struct ResolvedModelEntry {
    /// The preset name this entry came from.
    pub preset_name: String,
    /// The concrete LLM config.
    pub config: LlmConfig,
    /// Capability tier for filtering.
    pub tier: CapabilityTier,
}

/// The result of a routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    /// Selected provider.
    pub provider: String,
    /// Selected model.
    pub model: String,
    /// Name of the strategy that made this decision.
    pub strategy_name: String,
    /// Human-readable rationale.
    pub rationale: String,
    /// Fallback chain (preset_name, provider, model) if the primary fails.
    #[serde(default)]
    pub fallback_chain: Vec<(String, String, String)>,
    /// Whether a downgrade was applied due to budget pressure.
    #[serde(default)]
    pub was_downgraded: bool,
    /// Capability tier of the selected model (for approval gates).
    #[serde(default)]
    pub tier: Option<CapabilityTier>,
}

impl RoutingDecision {
    /// Check if this decision requires approval before proceeding.
    pub fn requires_approval(&self, gates: &ApprovalGatesConfig, ctx: &RoutingContext) -> bool {
        if gates.premium_model_first_use {
            let is_premium = self.tier == Some(CapabilityTier::Premium);
            let is_first_turn = ctx.time.turn_number.map(|t| t <= 1).unwrap_or(false);
            if is_premium && is_first_turn {
                return true;
            }
        }

        if let Some(threshold) = gates.budget_threshold_crossed {
            if let Some(pct) = ctx.budget.session_budget_used_pct {
                if pct >= threshold {
                    return true;
                }
            }
        }

        false
    }
}

/// Trait for model routing strategies.
#[async_trait::async_trait]
pub trait ModelRouter: Send + Sync {
    /// Select a model given the routing context, primary config, and resolved model list.
    async fn route(
        &self,
        ctx: &RoutingContext,
        primary_config: &LlmConfig,
        models: &[ResolvedModelEntry],
        routing_config: &LlmRoutingConfig,
    ) -> RoutingDecision;
}

/// Deterministic router — selects models based on budget + complexity signals.
#[derive(Debug, Clone, Default)]
pub struct DeterministicRouter;

impl DeterministicRouter {
    pub fn new() -> Self {
        Self
    }

    fn compute_effective_tier(
        &self,
        ctx: &RoutingContext,
        det_config: &DeterministicRoutingConfig,
        starting_max_tier: CapabilityTier,
    ) -> (CapabilityTier, bool) {
        let mut max_tier = starting_max_tier;
        let mut downgraded = false;

        if let Some(override_cost) = det_config.max_cost_usd {
            if let Some(cost) = ctx.budget.session_cost_usd {
                if cost >= override_cost && max_tier == CapabilityTier::Premium {
                    max_tier = CapabilityTier::Standard;
                    downgraded = true;
                }
            }
        }

        let threshold = det_config.budget_downgrade_threshold;
        if let Some(pct) = ctx.budget.session_budget_used_pct {
            if pct >= threshold && max_tier > CapabilityTier::Economy {
                max_tier = CapabilityTier::Economy;
                downgraded = true;
            }
        }

        if ctx.complexity.is_script_mode {
            if max_tier > CapabilityTier::Economy {
                max_tier = CapabilityTier::Economy;
                downgraded = true;
            }
        }

        (max_tier, downgraded)
    }
}

/// Build the fallback chain for a given tier, excluding the primary model.
fn build_fallback_chain(
    models: &[ResolvedModelEntry],
    max_tier: CapabilityTier,
    primary_provider: &str,
    primary_model: &str,
    enable_fallback: bool,
) -> Vec<(String, String, String)> {
    if !enable_fallback {
        return vec![];
    }

    models
        .iter()
        .filter(|m| {
            m.tier <= max_tier
                && !(m.config.provider == primary_provider && m.config.model == primary_model)
        })
        .map(|m| {
            (
                m.preset_name.clone(),
                m.config.provider.clone(),
                m.config.model.clone(),
            )
        })
        .collect()
}

#[async_trait::async_trait]
impl ModelRouter for DeterministicRouter {
    async fn route(
        &self,
        ctx: &RoutingContext,
        primary_config: &LlmConfig,
        models: &[ResolvedModelEntry],
        routing_config: &LlmRoutingConfig,
    ) -> RoutingDecision {
        let det_config = DeterministicRoutingConfig {
            max_tier: CapabilityTier::Premium,
            max_cost_usd: None,
            budget_downgrade_threshold: 0.8,
            enable_fallback_chain: true,
        };
        let mut max_tier = det_config.max_tier;

        if let Some(override_entry) = routing_config.agent_overrides.get(&ctx.agent_id) {
            if let Some(tier) = override_entry.min_tier {
                if max_tier < tier {
                    max_tier = tier;
                }
            }
            if let Some(ref model) = override_entry.model {
                let override_valid = models.iter().any(|m| {
                    m.config.model == *model && m.config.provider == primary_config.provider
                });
                if override_valid {
                    let override_tier = models
                        .iter()
                        .find(|m| {
                            m.config.model == *model && m.config.provider == primary_config.provider
                        })
                        .map(|m| m.tier);
                    return RoutingDecision {
                        provider: primary_config.provider.clone(),
                        model: model.clone(),
                        strategy_name: "agent_override".to_string(),
                        rationale: format!(
                            "agent override: forcing model {} for agent {}",
                            model, ctx.agent_id
                        ),
                        fallback_chain: vec![],
                        was_downgraded: false,
                        tier: override_tier,
                    };
                }
            }
        }

        let (effective_max_tier, was_downgraded) =
            self.compute_effective_tier(ctx, &det_config, max_tier);

        let primary_entry = models.iter().find(|m| {
            m.config.provider == primary_config.provider && m.config.model == primary_config.model
        });

        let (provider, model) = if let Some(entry) = primary_entry {
            if entry.tier <= effective_max_tier {
                (entry.config.provider.clone(), entry.config.model.clone())
            } else {
                let fallback = models
                    .iter()
                    .find(|m| m.tier <= effective_max_tier)
                    .or_else(|| models.iter().min_by_key(|m| m.tier));
                if let Some(fb) = fallback {
                    (fb.config.provider.clone(), fb.config.model.clone())
                } else {
                    (
                        primary_config.provider.clone(),
                        primary_config.model.clone(),
                    )
                }
            }
        } else {
            (
                primary_config.provider.clone(),
                primary_config.model.clone(),
            )
        };

        let fallback_chain = build_fallback_chain(
            models,
            effective_max_tier,
            &provider,
            &model,
            det_config.enable_fallback_chain,
        );

        let rationale = format!(
            "deterministic: effective_tier={}, budget_used={:.0}%, cost=${:.2}, downgraded={}",
            format!("{:?}", effective_max_tier).to_lowercase(),
            ctx.budget.session_budget_used_pct.unwrap_or(0.0) * 100.0,
            ctx.budget.session_cost_usd.unwrap_or(0.0),
            was_downgraded,
        );

        RoutingDecision {
            provider,
            model,
            strategy_name: "deterministic".to_string(),
            rationale,
            fallback_chain,
            was_downgraded,
            tier: Some(effective_max_tier),
        }
    }
}

/// Disabled router — always returns the primary config unchanged.
#[derive(Debug, Clone, Default)]
pub struct DisabledRouter;

#[async_trait::async_trait]
impl ModelRouter for DisabledRouter {
    async fn route(
        &self,
        _ctx: &RoutingContext,
        primary_config: &LlmConfig,
        _models: &[ResolvedModelEntry],
        _routing_config: &LlmRoutingConfig,
    ) -> RoutingDecision {
        RoutingDecision {
            provider: primary_config.provider.clone(),
            model: primary_config.model.clone(),
            strategy_name: "disabled".to_string(),
            rationale: "routing disabled — using primary model".to_string(),
            fallback_chain: vec![],
            was_downgraded: false,
            tier: None,
        }
    }
}

const CLASSIFIER_PROMPT: &str = r#"Classify the complexity of this conversation to select an appropriate model tier.

Tiers:
- economy: Simple Q&A, factual lookup, classification, summarization, data extraction
- standard: Reasoning with tool use, code generation, multi-step planning, analysis
- premium: Complex reasoning, architecture design, security review, ambiguous requirements

Respond with ONLY one word: "economy", "standard", or "premium"."#;

/// LLM Classifier Router — uses a cheap model to classify request complexity
/// and select the appropriate tier. Falls back to deterministic routing on
/// timeout or error.
#[derive(Debug, Clone)]
pub struct LlmClassifierRouter {
    classifier_config: LlmConfig,
    timeout_secs: u64,
    skip_threshold: f32,
}

impl LlmClassifierRouter {
    pub fn new(classifier_config: LlmConfig, timeout_secs: u64, skip_threshold: f32) -> Self {
        Self {
            classifier_config,
            timeout_secs: if timeout_secs == 0 { 2 } else { timeout_secs },
            skip_threshold,
        }
    }

    fn build_classification_prompt(ctx: &RoutingContext) -> String {
        format!(
            "Conversation context:\n- Turn: {}\n- Tool count: {:?}\n- Has workflow caps: {}\n- Has artifact caps: {}\n- Script mode: {}",
            ctx.time.turn_number.unwrap_or(0),
            ctx.complexity.tool_count,
            ctx.complexity.has_workflow_caps,
            ctx.complexity.has_artifact_caps,
            ctx.complexity.is_script_mode,
        )
    }

    fn tier_from_response(response_text: &str) -> Option<CapabilityTier> {
        let text = response_text.trim().to_lowercase();
        if text.contains("economy") {
            Some(CapabilityTier::Economy)
        } else if text.contains("standard") {
            Some(CapabilityTier::Standard)
        } else if text.contains("premium") {
            Some(CapabilityTier::Premium)
        } else {
            None
        }
    }

    fn select_model_for_tier(
        &self,
        tier: CapabilityTier,
        models: &[ResolvedModelEntry],
        primary_config: &LlmConfig,
    ) -> (String, String) {
        let entry = models
            .iter()
            .find(|m| m.tier == tier && m.config.provider == primary_config.provider)
            .or_else(|| models.iter().find(|m| m.tier == tier))
            .or_else(|| {
                models
                    .iter()
                    .filter(|m| m.tier <= tier)
                    .max_by_key(|m| m.tier)
            });
        if let Some(entry) = entry {
            (entry.config.provider.clone(), entry.config.model.clone())
        } else {
            (
                primary_config.provider.clone(),
                primary_config.model.clone(),
            )
        }
    }
}

#[async_trait::async_trait]
impl ModelRouter for LlmClassifierRouter {
    async fn route(
        &self,
        ctx: &RoutingContext,
        primary_config: &LlmConfig,
        models: &[ResolvedModelEntry],
        routing_config: &LlmRoutingConfig,
    ) -> RoutingDecision {
        let skip_threshold = self.skip_threshold;

        if let Some(pct) = ctx.budget.session_budget_used_pct {
            if pct >= skip_threshold {
                tracing::debug!(
                    target: "autonoetic::model_routing",
                    "Skipping LLM classifier due to extreme budget pressure"
                );
                return DeterministicRouter::new()
                    .route(ctx, primary_config, models, routing_config)
                    .await;
            }
        }

        let prompt = Self::build_classification_prompt(ctx);
        let messages = vec![
            Message {
                role: Role::System,
                content: CLASSIFIER_PROMPT.to_string(),
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
            Message {
                role: Role::User,
                content: prompt,
                tool_calls: vec![],
                tool_call_id: None,
                reasoning_content: None,
                reasoning_details: None,
            },
        ];

        let req = CompletionRequest {
            model: self.classifier_config.model.clone(),
            messages,
            tools: vec![],
            max_tokens: Some(20),
            temperature: Some(0.0),
            metadata: None,
            thinking: None,
            prompt_cache_key: None,
            system_cache_prefix_bytes: None,
        };

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            self.call_classifier(&self.classifier_config, &req),
        )
        .await;

        match result {
            Ok(Ok(response)) => {
                if let Some(tier) = Self::tier_from_response(&response.text) {
                    let (provider, model) =
                        self.select_model_for_tier(tier, models, primary_config);
                    let fallback_chain =
                        build_fallback_chain(models, tier, &provider, &model, true);

                    let rationale = format!(
                        "classifier: tier={}, classifier_model={}",
                        format!("{:?}", tier).to_lowercase(),
                        self.classifier_config.model
                    );

                    return RoutingDecision {
                        provider,
                        model,
                        strategy_name: "classifier".to_string(),
                        rationale,
                        fallback_chain,
                        was_downgraded: false,
                        tier: Some(tier),
                    };
                }
                tracing::warn!(
                    target: "autonoetic::model_routing",
                    "Classifier returned unparsable response, falling back to deterministic"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    target: "autonoetic::model_routing",
                    error = %e,
                    "Classifier call failed, falling back to deterministic"
                );
            }
            Err(_) => {
                tracing::warn!(
                    target: "autonoetic::model_routing",
                    timeout_secs = self.timeout_secs,
                    "Classifier timed out, falling back to deterministic"
                );
            }
        }

        DeterministicRouter::new()
            .route(ctx, primary_config, models, routing_config)
            .await
    }
}

impl LlmClassifierRouter {
    async fn call_classifier(
        &self,
        config: &LlmConfig,
        req: &CompletionRequest,
    ) -> anyhow::Result<crate::llm::CompletionResponse> {
        let driver = crate::llm::build_driver(config.clone(), reqwest::Client::new())?;
        driver.complete(req).await
    }
}

/// Hybrid Router — uses deterministic routing first, then consults
/// the LLM classifier only when the deterministic confidence is below
/// the ambiguity threshold.
#[derive(Debug, Clone)]
pub struct HybridRouter {
    deterministic: DeterministicRouter,
    classifier: LlmClassifierRouter,
    ambiguity_threshold: f32,
}

impl HybridRouter {
    pub fn new(
        classifier_config: LlmConfig,
        classifier_timeout: u64,
        classifier_skip: f32,
        ambiguity_threshold: f32,
    ) -> Self {
        Self {
            deterministic: DeterministicRouter::new(),
            classifier: LlmClassifierRouter::new(
                classifier_config,
                classifier_timeout,
                classifier_skip,
            ),
            ambiguity_threshold: if ambiguity_threshold == 0.0 {
                0.5
            } else {
                ambiguity_threshold
            },
        }
    }

    fn compute_ambiguity(ctx: &RoutingContext) -> f32 {
        let mut ambiguity = 0.0;

        if ctx.complexity.tool_count.unwrap_or(0) > 5 {
            ambiguity += 0.2;
        }
        if ctx.complexity.has_workflow_caps {
            ambiguity += 0.15;
        }
        if ctx.complexity.has_artifact_caps {
            ambiguity += 0.15;
        }
        if ctx.complexity.is_script_mode {
            ambiguity = 0.0;
        }

        if let Some(pct) = ctx.budget.session_budget_used_pct {
            if pct > 0.5 {
                ambiguity += 0.1 * pct;
            }
        }

        ambiguity.min(1.0)
    }
}

#[async_trait::async_trait]
impl ModelRouter for HybridRouter {
    async fn route(
        &self,
        ctx: &RoutingContext,
        primary_config: &LlmConfig,
        models: &[ResolvedModelEntry],
        routing_config: &LlmRoutingConfig,
    ) -> RoutingDecision {
        let ambiguity = Self::compute_ambiguity(ctx);

        if ambiguity < self.ambiguity_threshold {
            self.deterministic
                .route(ctx, primary_config, models, routing_config)
                .await
        } else {
            let mut decision = self
                .classifier
                .route(ctx, primary_config, models, routing_config)
                .await;
            decision.strategy_name = "hybrid".to_string();
            decision.rationale = format!(
                "hybrid: ambiguity={:.2}, threshold={:.2}, {}",
                ambiguity, self.ambiguity_threshold, decision.rationale
            );
            decision
        }
    }
}

/// Create router from a routing preset config.
/// `resolved_models`: (preset_name, LlmConfig, tier) tuples from resolving the preset's models list.
/// `classifier_config`: resolved LlmConfig for the classifier, if applicable.
pub fn create_router_from_preset(
    preset: &RoutingPresetConfig,
    resolved_models: Vec<ResolvedModelEntry>,
    classifier_config: Option<LlmConfig>,
) -> (Box<dyn ModelRouter>, Vec<ResolvedModelEntry>) {
    match preset.strategy {
        RoutingStrategy::Disabled => (Box::new(DisabledRouter), resolved_models),
        RoutingStrategy::Deterministic => (Box::new(DeterministicRouter::new()), resolved_models),
        RoutingStrategy::Classifier => {
            let classifier = classifier_config.unwrap_or_else(|| LlmConfig {
                provider: "anthropic".to_string(),
                model: "claude-haiku-3-20250307".to_string(),
                temperature: 0.0,
                fallback_provider: None,
                fallback_model: None,
                chat_only: false,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                routing_preset: None,
                thinking: None,
                egress_class: None,
            });
            (
                Box::new(LlmClassifierRouter::new(
                    classifier,
                    preset.classifier.timeout_secs,
                    preset.classifier.skip_threshold,
                )),
                resolved_models,
            )
        }
        RoutingStrategy::Hybrid => {
            let classifier = classifier_config.unwrap_or_else(|| LlmConfig {
                provider: "anthropic".to_string(),
                model: "claude-haiku-3-20250307".to_string(),
                temperature: 0.0,
                fallback_provider: None,
                fallback_model: None,
                chat_only: false,
                context_window_tokens: None,
                base_url: None,
                api_key_env: None,
                routing_preset: None,
                thinking: None,
                egress_class: None,
            });
            (
                Box::new(HybridRouter::new(
                    classifier,
                    preset.classifier.timeout_secs,
                    preset.classifier.skip_threshold,
                    preset.hybrid.ambiguity_threshold,
                )),
                resolved_models,
            )
        }
    }
}

/// Build an LlmConfig from a routing decision, optionally carrying
/// model-specific overrides (context window, base URL).
pub fn decision_to_llm_config(
    decision: &RoutingDecision,
    base_config: &LlmConfig,
    model_entry: Option<&ResolvedModelEntry>,
) -> LlmConfig {
    LlmConfig {
        provider: decision.provider.clone(),
        model: decision.model.clone(),
        temperature: base_config.temperature,
        fallback_provider: base_config.fallback_provider.clone(),
        fallback_model: base_config.fallback_model.clone(),
        chat_only: base_config.chat_only,
        context_window_tokens: model_entry
            .and_then(|e| e.config.context_window_tokens)
            .or(base_config.context_window_tokens),
        base_url: model_entry
            .and_then(|e| e.config.base_url.clone())
            .or(base_config.base_url.clone()),
        api_key_env: base_config.api_key_env.clone(),
        routing_preset: base_config.routing_preset.clone(),
        thinking: base_config.thinking.clone(),
        egress_class: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::config::{
        ApprovalGatesConfig, BudgetState, CapabilityTier, ComplexitySignals,
        DeterministicRoutingConfig, ModelCost, RoutingStrategy, TimeSignals,
    };

    fn resolved_models() -> Vec<ResolvedModelEntry> {
        vec![
            ResolvedModelEntry {
                preset_name: "opus".to_string(),
                config: LlmConfig {
                    provider: "anthropic".to_string(),
                    model: "claude-opus-4-20250514".to_string(),
                    temperature: 0.2,
                    fallback_provider: None,
                    fallback_model: None,
                    chat_only: false,
                    context_window_tokens: None,
                    base_url: None,
                    api_key_env: None,
                    routing_preset: None,
                    thinking: None,
                    egress_class: None,
                },
                tier: CapabilityTier::Premium,
            },
            ResolvedModelEntry {
                preset_name: "sonnet".to_string(),
                config: LlmConfig {
                    provider: "anthropic".to_string(),
                    model: "claude-sonnet-4-20250514".to_string(),
                    temperature: 0.2,
                    fallback_provider: None,
                    fallback_model: None,
                    chat_only: false,
                    context_window_tokens: None,
                    base_url: None,
                    api_key_env: None,
                    routing_preset: None,
                    thinking: None,
                    egress_class: None,
                },
                tier: CapabilityTier::Standard,
            },
            ResolvedModelEntry {
                preset_name: "haiku".to_string(),
                config: LlmConfig {
                    provider: "anthropic".to_string(),
                    model: "claude-haiku-3-20250307".to_string(),
                    temperature: 0.0,
                    fallback_provider: None,
                    fallback_model: None,
                    chat_only: false,
                    context_window_tokens: None,
                    base_url: None,
                    api_key_env: None,
                    routing_preset: None,
                    thinking: None,
                    egress_class: None,
                },
                tier: CapabilityTier::Economy,
            },
        ]
    }

    fn primary_config() -> LlmConfig {
        LlmConfig {
            provider: "anthropic".to_string(),
            model: "claude-opus-4-20250514".to_string(),
            temperature: 0.2,
            fallback_provider: None,
            fallback_model: None,
            chat_only: false,
            context_window_tokens: None,
            base_url: None,
            api_key_env: None,
            routing_preset: None,
            thinking: None,
            egress_class: None,
        }
    }

    fn routing_config() -> LlmRoutingConfig {
        LlmRoutingConfig::default()
    }

    #[tokio::test]
    async fn test_deterministic_router_selects_primary_when_budget_low() {
        let router = DeterministicRouter::new();
        let ctx = RoutingContext {
            agent_id: "test-agent".to_string(),
            session_id: "test-session".to_string(),
            budget: BudgetState {
                session_budget_used_pct: Some(0.3),
                prompt_budget_used_pct: Some(0.2),
                session_cost_usd: Some(1.0),
            },
            complexity: ComplexitySignals::default(),
            time: TimeSignals::default(),
        };
        let models = resolved_models();
        let decision = router
            .route(&ctx, &primary_config(), &models, &routing_config())
            .await;

        assert_eq!(decision.provider, "anthropic");
        assert_eq!(decision.model, "claude-opus-4-20250514");
        assert!(!decision.was_downgraded);
        assert_eq!(decision.strategy_name, "deterministic");
    }

    #[tokio::test]
    async fn test_deterministic_router_downgrades_on_budget_pressure() {
        let router = DeterministicRouter::new();
        let ctx = RoutingContext {
            agent_id: "test-agent".to_string(),
            session_id: "test-session".to_string(),
            budget: BudgetState {
                session_budget_used_pct: Some(0.9),
                prompt_budget_used_pct: Some(0.7),
                session_cost_usd: Some(5.0),
            },
            complexity: ComplexitySignals::default(),
            time: TimeSignals::default(),
        };
        let models = resolved_models();
        let decision = router
            .route(&ctx, &primary_config(), &models, &routing_config())
            .await;

        assert_eq!(decision.model, "claude-haiku-3-20250307");
        assert!(decision.was_downgraded);
    }

    #[tokio::test]
    async fn test_disabled_router_returns_primary() {
        let router = DisabledRouter;
        let ctx = RoutingContext::default();
        let models = resolved_models();
        let decision = router
            .route(&ctx, &primary_config(), &models, &routing_config())
            .await;

        assert_eq!(decision.provider, "anthropic");
        assert_eq!(decision.model, "claude-opus-4-20250514");
        assert_eq!(decision.strategy_name, "disabled");
    }

    #[tokio::test]
    async fn test_deterministic_router_respects_budget_pressure() {
        let router = DeterministicRouter::new();
        let ctx = RoutingContext {
            agent_id: "test-agent".to_string(),
            session_id: "test-session".to_string(),
            budget: BudgetState {
                session_budget_used_pct: Some(0.85),
                prompt_budget_used_pct: Some(0.7),
                session_cost_usd: Some(15.0),
            },
            complexity: ComplexitySignals::default(),
            time: TimeSignals::default(),
        };
        let models = resolved_models();
        let decision = router
            .route(&ctx, &primary_config(), &models, &routing_config())
            .await;

        assert_eq!(decision.model, "claude-haiku-3-20250307");
        assert!(decision.was_downgraded);
    }

    #[tokio::test]
    async fn test_fallback_chain_excludes_primary() {
        let models = resolved_models();
        let chain = build_fallback_chain(
            &models,
            CapabilityTier::Premium,
            "anthropic",
            "claude-opus-4-20250514",
            true,
        );

        assert_eq!(chain.len(), 2);
        assert!(!chain
            .iter()
            .any(|(_, p, m)| p == "anthropic" && m == "claude-opus-4-20250514"));
    }

    #[tokio::test]
    async fn test_agent_override_forces_model() {
        let router = DeterministicRouter::new();
        let ctx = RoutingContext {
            agent_id: "coder.default".to_string(),
            session_id: "test-session".to_string(),
            budget: BudgetState {
                session_budget_used_pct: Some(0.3),
                prompt_budget_used_pct: Some(0.2),
                session_cost_usd: Some(1.0),
            },
            complexity: ComplexitySignals::default(),
            time: TimeSignals::default(),
        };
        let models = resolved_models();
        let mut rc = routing_config();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "coder.default".to_string(),
            autonoetic_types::config::ModelOverride {
                model: Some("claude-sonnet-4-20250514".to_string()),
                min_tier: None,
            },
        );
        rc.agent_overrides = overrides;

        let decision = router.route(&ctx, &primary_config(), &models, &rc).await;

        assert_eq!(decision.model, "claude-sonnet-4-20250514");
        assert_eq!(decision.strategy_name, "agent_override");
    }
}
