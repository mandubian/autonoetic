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
pub mod schema_compress;
pub mod strategies;
pub mod trimming;

/// Configuration for the context governor pipeline.
pub struct GovernorConfig {
    pub http_client: reqwest::Client,
    pub presets: HashMap<String, LlmPreset>,
    pub gateway_dir: Option<PathBuf>,
}

/// The context governor — runs a pluggable strategy pipeline.
pub struct ContextGovernor {
    strategies: Vec<Box<dyn ReductionStrategy>>,
}

use std::path::PathBuf;

impl ContextGovernor {
    /// Build the default pipeline.
    ///
    /// Order: history trimming → capsule summarization → schema compression
    /// → tool demotion.
    ///
    /// Schema compression is intentionally late in the pipeline: stripping
    /// tool schemas damages the LLM's ability to call tools correctly (wrong
    /// parameter names, missing required fields, hallucinated arguments).
    /// It should only run when trimming and summarization are insufficient.
    pub fn new(config: &GovernorConfig) -> Self {
        let mut capsule = capsule::CapsuleStrategy::new(
            config.http_client.clone(),
            config.presets.clone(),
        );
        if let Some(ref dir) = config.gateway_dir {
            capsule = capsule.with_gateway_dir(dir.clone());
        }
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            Box::new(trimming::TrimHistoryStrategy),
            Box::new(capsule),
            Box::new(schema_compress::ToolSchemaCompressionStrategy::new()),
            Box::new(demotion::ToolDemotionStrategy),
        ];
        Self { strategies }
    }

    /// Build the aggressive pipeline for overflow recovery retry.
    ///
    /// Skips the LLM-tier capsule strategy (already attempted) and forces
    /// schema compression on turn 0 + goes straight to lossy reduction
    /// (trim + demote).
    pub fn new_aggressive(config: &GovernorConfig) -> Self {
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            // Skip capsule (already attempted in prior run)
            Box::new(trimming::TrimHistoryStrategy),
            // Force schema compression on every turn (even turn 0)
            Box::new(schema_compress::ToolSchemaCompressionStrategy::forced()),
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
