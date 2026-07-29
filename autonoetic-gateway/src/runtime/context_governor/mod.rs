//! Pluggable Context Governor.
//!
//! Owns the full context budget lifecycle as a configurable pipeline of
//! reduction strategies. The lifecycle loop calls a single `govern()` method.
//!
//! Adding a new reduction approach: implement `ReductionStrategy`, register
//! it in the pipeline. Zero changes to lifecycle or the pipeline orchestrator.

use crate::runtime::context_governor::error::{BudgetSnapshot, GovernorAction, RecoveryAction};
use crate::runtime::context_governor::strategies::{
    GovernorContext, GovernorResult, ReductionOutcome, ReductionStrategy,
};
use autonoetic_types::config::LlmPreset;
use std::collections::HashMap;

pub mod budget;
pub mod capsule;
pub mod demotion;
pub mod error;
pub mod resolver;
pub mod strategies;
pub mod trimming;

/// Configuration for the context governor pipeline.
pub struct GovernorConfig {
    pub http_client: reqwest::Client,
    pub presets: HashMap<String, LlmPreset>,
    pub gateway_dir: Option<PathBuf>,
    /// Optional store for durable egress events (`egress.boundary_refused`,
    /// synthesized `egress.envelope_labeled`) from the capsule strategy.
    pub gateway_store: Option<std::sync::Arc<crate::scheduler::gateway_store::GatewayStore>>,
    /// Agent id attributed on those egress events (`None` → `"unknown"`).
    pub agent_id: Option<String>,
}

/// The context governor — runs a pluggable strategy pipeline.
pub struct ContextGovernor {
    strategies: Vec<Box<dyn ReductionStrategy>>,
}

use std::path::PathBuf;

impl ContextGovernor {
    /// Build the default pipeline.
    ///
    /// Order: history trimming → capsule summarization → tool demotion.
    pub fn new(config: &GovernorConfig) -> Self {
        let mut capsule = capsule::CapsuleStrategy::new(
            config.http_client.clone(),
            config.presets.clone(),
        );
        if let Some(ref dir) = config.gateway_dir {
            capsule = capsule.with_gateway_dir(dir.clone());
        }
        if let Some(ref store) = config.gateway_store {
            capsule = capsule.with_gateway_store(store.clone());
        }
        if let Some(ref agent_id) = config.agent_id {
            capsule = capsule.with_agent_id(agent_id.clone());
        }
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            Box::new(trimming::TrimHistoryStrategy),
            Box::new(capsule),
            Box::new(demotion::ToolDemotionStrategy),
        ];
        Self { strategies }
    }

    /// Build the aggressive pipeline for overflow recovery retry.
    ///
    /// Skips the LLM-tier capsule strategy (already attempted) and goes
    /// straight to lossy reduction (trim + demote).
    pub fn new_aggressive(config: &GovernorConfig) -> Self {
        let _ = config; // no capsule to build in the aggressive path
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            // Skip capsule (already attempted in prior run)
            Box::new(trimming::TrimHistoryStrategy),
            Box::new(demotion::ToolDemotionStrategy),
        ];
        Self { strategies }
    }

    /// Build a custom pipeline (for testing or per-profile overrides).
    pub fn with_strategies(strategies: Vec<Box<dyn ReductionStrategy>>) -> Self {
        Self { strategies }
    }

    /// Run the full pipeline.
    ///
    /// Returns `WithinBudget` if no action needed, `Recovered` if the pipeline
    /// resolved the issue, or `Overflow` with diagnostics if all strategies
    /// were exhausted.
    ///
    /// A configured `soft_budget_tokens` triggers the pipeline proactively,
    /// even when the hard `effective_limit` has not been reached. This is
    /// useful for large context-window models where waiting for the hard
    /// limit wastes tokens on every round.
    pub async fn govern(&self, ctx: &mut GovernorContext) -> anyhow::Result<GovernorResult> {
        let hard_ok = ctx.breakdown.total_tokens <= ctx.effective_limit;
        let soft_limit = ctx.budget_config.soft_budget_tokens.map(|sb| sb as usize);
        let soft_ok = soft_limit
            .map(|sb| ctx.breakdown.total_tokens <= sb)
            .unwrap_or(true);

        if hard_ok && soft_ok {
            return Ok(GovernorResult::WithinBudget);
        }

        // If the soft budget is the binding constraint, temporarily lower the
        // limit the strategies target so they actually reduce down to the soft
        // budget instead of returning Resolved immediately because the total is
        // still below the hard context-window limit.
        let original_effective_limit = ctx.effective_limit;
        if let Some(sb) = soft_limit {
            if sb < ctx.effective_limit {
                tracing::debug!(
                    target: "autonoetic::context_governor",
                    soft_budget_tokens = sb,
                    original_effective_limit,
                    total_tokens = ctx.breakdown.total_tokens,
                    "Soft budget is binding; strategies will target soft limit"
                );
                ctx.effective_limit = sb;
            }
        }

        let mut actions_taken: Vec<GovernorAction> = Vec::new();

        for strategy in &self.strategies {
            let outcome = strategy.reduce(ctx).await?;
            match outcome {
                ReductionOutcome::Resolved { tokens_after } => {
                    actions_taken.push(GovernorAction {
                        strategy: strategy.name().to_string(),
                        tokens_after,
                    });
                    ctx.effective_limit = original_effective_limit;
                    return Ok(GovernorResult::Recovered { actions_taken });
                }
                ReductionOutcome::Insufficient { tokens_remaining } => {
                    actions_taken.push(GovernorAction {
                        strategy: strategy.name().to_string(),
                        tokens_after: tokens_remaining,
                    });
                }
            }
        }

        ctx.effective_limit = original_effective_limit;
        Ok(GovernorResult::Overflow(
            crate::runtime::context_governor::error::ContextOverflowDiagnostic {
                budget_snapshot: BudgetSnapshot::from(ctx),
                actions_attempted: actions_taken,
                recovery_action: RecoveryAction::Failed,
            },
        ))
    }
}

impl From<&GovernorContext> for BudgetSnapshot {
    fn from(ctx: &GovernorContext) -> Self {
        BudgetSnapshot {
            estimated_input: ctx.breakdown.total_tokens,
            margin: ctx.budget_config.margin_tokens,
            window: ctx.breakdown.context_window,
            threshold_pct: ctx.budget_config.warn_at_pct,
        }
    }
}

impl From<&mut GovernorContext> for BudgetSnapshot {
    fn from(ctx: &mut GovernorContext) -> Self {
        BudgetSnapshot {
            estimated_input: ctx.breakdown.total_tokens,
            margin: ctx.budget_config.margin_tokens,
            window: ctx.breakdown.context_window,
            threshold_pct: ctx.budget_config.warn_at_pct,
        }
    }
}
