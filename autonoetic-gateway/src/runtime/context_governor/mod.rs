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
pub mod compression;
pub mod demotion;
pub mod error;
pub mod resolver;
pub mod schema_compress;
pub mod strategies;
pub mod trimming;

const STRICT_GOVERNOR_ENV: &str = "AUTONOETIC_STRICT_CONTEXT_GOVERNOR";

/// Whether the strict context governor is enabled.
pub fn strict_governor_enabled() -> bool {
    std::env::var(STRICT_GOVERNOR_ENV).as_deref() == Ok("1")
}

/// Configuration for the context governor pipeline.
pub struct GovernorConfig {
    pub http_client: reqwest::Client,
    pub presets: HashMap<String, LlmPreset>,
}

/// The context governor — runs a pluggable strategy pipeline.
pub struct ContextGovernor {
    strategies: Vec<Box<dyn ReductionStrategy>>,
}

impl ContextGovernor {
    /// Build the default pipeline.
    pub fn new(config: &GovernorConfig) -> Self {
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            Box::new(schema_compress::ToolSchemaCompressionStrategy),
            Box::new(compression::CompressionStrategy::new(
                config.http_client.clone(),
                config.presets.clone(),
            )),
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
    pub async fn govern(&self, ctx: &mut GovernorContext) -> anyhow::Result<GovernorResult> {
        let within_budget = ctx.breakdown.total_tokens <= ctx.effective_limit;
        if within_budget {
            return Ok(GovernorResult::WithinBudget);
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
