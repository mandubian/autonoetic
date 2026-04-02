//! Smart model routing for LLM completions.
//!
//! Provides deterministic model selection based on budget pressure,
//! task complexity, and cost constraints. Falls back through a
//! configured chain on failure.

use autonoetic_types::agent::LlmConfig;
use autonoetic_types::config::{
    ApprovalGatesConfig, BudgetState, CapabilityTier, ComplexitySignals,
    DeterministicRoutingConfig, LlmRoutingConfig, ModelEntry, RoutingContext, RoutingStrategy,
    TimeSignals,
};
use serde::{Deserialize, Serialize};

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
    /// Fallback chain (provider/model pairs) if the primary fails.
    #[serde(default)]
    pub fallback_chain: Vec<(String, String)>,
    /// Whether a downgrade was applied due to budget pressure.
    #[serde(default)]
    pub was_downgraded: bool,
}

/// Trait for model routing strategies.
#[async_trait::async_trait]
pub trait ModelRouter: Send + Sync {
    /// Select a model given the routing context and available models.
    async fn route(
        &self,
        ctx: &RoutingContext,
        primary_config: &LlmConfig,
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

    fn build_fallback_chain(
        &self,
        routing_config: &LlmRoutingConfig,
        max_tier: CapabilityTier,
        primary_provider: &str,
        primary_model: &str,
    ) -> Vec<(String, String)> {
        if !routing_config.deterministic.enable_fallback_chain {
            return vec![];
        }

        routing_config
            .models
            .iter()
            .filter(|m| {
                m.tier <= max_tier && !(m.provider == primary_provider && m.model == primary_model)
            })
            .map(|m| (m.provider.clone(), m.model.clone()))
            .collect()
    }
}

#[async_trait::async_trait]
impl ModelRouter for DeterministicRouter {
    async fn route(
        &self,
        ctx: &RoutingContext,
        primary_config: &LlmConfig,
        routing_config: &LlmRoutingConfig,
    ) -> RoutingDecision {
        let mut max_tier = routing_config.deterministic.max_tier;

        if let Some(override_entry) = routing_config.agent_overrides.get(&ctx.agent_id) {
            if let Some(tier) = override_entry.min_tier {
                if max_tier < tier {
                    max_tier = tier;
                }
            }
            if let Some(ref model) = override_entry.model {
                let override_valid = routing_config
                    .models
                    .iter()
                    .any(|m| m.model == *model && m.provider == primary_config.provider);
                if override_valid {
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
                    };
                }
            }
        }

        let (effective_max_tier, was_downgraded) =
            self.compute_effective_tier(ctx, &routing_config.deterministic, max_tier);

        let primary_entry = routing_config
            .models
            .iter()
            .find(|m| m.provider == primary_config.provider && m.model == primary_config.model);

        let (provider, model) = if let Some(entry) = primary_entry {
            if entry.tier <= effective_max_tier {
                (entry.provider.clone(), entry.model.clone())
            } else {
                let fallback = routing_config
                    .models
                    .iter()
                    .find(|m| m.tier <= effective_max_tier)
                    .or_else(|| routing_config.models.iter().min_by_key(|m| m.tier));
                if let Some(fb) = fallback {
                    (fb.provider.clone(), fb.model.clone())
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

        let fallback_chain =
            self.build_fallback_chain(routing_config, effective_max_tier, &provider, &model);

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
        _routing_config: &LlmRoutingConfig,
    ) -> RoutingDecision {
        RoutingDecision {
            provider: primary_config.provider.clone(),
            model: primary_config.model.clone(),
            strategy_name: "disabled".to_string(),
            rationale: "routing disabled — using primary model".to_string(),
            fallback_chain: vec![],
            was_downgraded: false,
        }
    }
}

/// Factory function to create the appropriate router based on config.
pub fn create_router(strategy: RoutingStrategy) -> Box<dyn ModelRouter> {
    match strategy {
        RoutingStrategy::Disabled => Box::new(DisabledRouter),
        RoutingStrategy::Deterministic => Box::new(DeterministicRouter::new()),
    }
}

/// Build an LlmConfig from a routing decision, optionally carrying
/// model-specific overrides (context window, base URL).
pub fn decision_to_llm_config(
    decision: &RoutingDecision,
    base_config: &LlmConfig,
    model_entry: Option<&ModelEntry>,
) -> LlmConfig {
    LlmConfig {
        provider: decision.provider.clone(),
        model: decision.model.clone(),
        temperature: base_config.temperature,
        fallback_provider: base_config.fallback_provider.clone(),
        fallback_model: base_config.fallback_model.clone(),
        chat_only: base_config.chat_only,
        context_window_tokens: model_entry
            .and_then(|e| e.context_window_tokens)
            .or(base_config.context_window_tokens),
        base_url: model_entry
            .and_then(|e| e.base_url.clone())
            .or(base_config.base_url.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::config::{
        ApprovalGatesConfig, BudgetState, ComplexitySignals, DeterministicRoutingConfig,
        LlmRoutingConfig, ModelCost, ModelEntry, RoutingStrategy, TimeSignals,
    };

    fn routing_config() -> LlmRoutingConfig {
        LlmRoutingConfig {
            strategy: RoutingStrategy::Deterministic,
            models: vec![
                ModelEntry {
                    provider: "anthropic".to_string(),
                    model: "claude-opus-4-20250514".to_string(),
                    tier: CapabilityTier::Premium,
                    cost: Some(ModelCost {
                        input_per_million: Some(15.0),
                        output_per_million: Some(75.0),
                    }),
                    latency: None,
                    context_window_tokens: None,
                    base_url: None,
                },
                ModelEntry {
                    provider: "anthropic".to_string(),
                    model: "claude-sonnet-4-20250514".to_string(),
                    tier: CapabilityTier::Standard,
                    cost: Some(ModelCost {
                        input_per_million: Some(3.0),
                        output_per_million: Some(15.0),
                    }),
                    latency: None,
                    context_window_tokens: None,
                    base_url: None,
                },
                ModelEntry {
                    provider: "anthropic".to_string(),
                    model: "claude-haiku-3-20250307".to_string(),
                    tier: CapabilityTier::Economy,
                    cost: Some(ModelCost {
                        input_per_million: Some(0.25),
                        output_per_million: Some(1.25),
                    }),
                    latency: None,
                    context_window_tokens: None,
                    base_url: None,
                },
            ],
            deterministic: DeterministicRoutingConfig {
                max_tier: CapabilityTier::Premium,
                max_cost_usd: Some(10.0),
                budget_downgrade_threshold: 0.8,
                enable_fallback_chain: true,
            },
            agent_overrides: std::collections::HashMap::new(),
            approval_gates: ApprovalGatesConfig::default(),
        }
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
        }
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
        let config = routing_config();
        let decision = router.route(&ctx, &primary_config(), &config).await;

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
        let config = routing_config();
        let decision = router.route(&ctx, &primary_config(), &config).await;

        assert!(decision.was_downgraded);
        assert_eq!(decision.model, "claude-haiku-3-20250307");
    }

    #[tokio::test]
    async fn test_deterministic_router_downgrades_on_cost_threshold() {
        let router = DeterministicRouter::new();
        let ctx = RoutingContext {
            agent_id: "test-agent".to_string(),
            session_id: "test-session".to_string(),
            budget: BudgetState {
                session_budget_used_pct: Some(0.5),
                prompt_budget_used_pct: Some(0.3),
                session_cost_usd: Some(12.0),
            },
            complexity: ComplexitySignals::default(),
            time: TimeSignals::default(),
        };
        let config = routing_config();
        let decision = router.route(&ctx, &primary_config(), &config).await;

        assert!(decision.was_downgraded);
    }

    #[tokio::test]
    async fn test_deterministic_router_includes_fallback_chain() {
        let router = DeterministicRouter::new();
        let ctx = RoutingContext::default();
        let config = routing_config();
        let decision = router.route(&ctx, &primary_config(), &config).await;

        assert!(!decision.fallback_chain.is_empty());
        assert!(decision
            .fallback_chain
            .iter()
            .any(|(p, m)| { p == "anthropic" && m == "claude-sonnet-4-20250514" }));
    }

    #[tokio::test]
    async fn test_disabled_router_always_returns_primary() {
        let router = DisabledRouter;
        let ctx = RoutingContext {
            budget: BudgetState {
                session_budget_used_pct: Some(0.99),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = routing_config();
        let decision = router.route(&ctx, &primary_config(), &config).await;

        assert_eq!(decision.provider, "anthropic");
        assert_eq!(decision.model, "claude-opus-4-20250514");
        assert!(!decision.was_downgraded);
    }

    #[test]
    fn test_decision_to_llm_config() {
        let decision = RoutingDecision {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            strategy_name: "deterministic".to_string(),
            rationale: "test".to_string(),
            fallback_chain: vec![],
            was_downgraded: true,
        };
        let base = primary_config();
        let new_config = decision_to_llm_config(&decision, &base, None);

        assert_eq!(new_config.provider, "anthropic");
        assert_eq!(new_config.model, "claude-sonnet-4-20250514");
        assert_eq!(new_config.temperature, base.temperature);
    }

    #[tokio::test]
    async fn test_script_mode_forces_economy_tier() {
        let router = DeterministicRouter::new();
        let ctx = RoutingContext {
            complexity: ComplexitySignals {
                is_script_mode: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let config = routing_config();
        let decision = router.route(&ctx, &primary_config(), &config).await;

        assert!(decision.was_downgraded);
    }
}
