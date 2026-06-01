use crate::llm::{Message, ToolDefinition};
use crate::runtime::compression::CompressionMetadata;
use crate::runtime::context_governor::capsule::StateCapsule;
use crate::runtime::context_governor::error::GovernorAction;
use crate::runtime::prompt_budget::PromptBudgetBreakdown;
use autonoetic_types::agent::CompressionConfig;
use autonoetic_types::config::{ContextCompressionConfig, PromptBudgetConfig};
use autonoetic_types::plan_frame::PlanFrameSummary;

/// Shared mutable state passed through the reduction pipeline.
pub struct GovernorContext {
    pub history: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub breakdown: PromptBudgetBreakdown,
    pub effective_limit: usize,
    pub turn_number: u64,
    pub session_id: String,
    pub compression_metadata: Option<CompressionMetadata>,
    pub budget_config: PromptBudgetConfig,
    pub compression_config: Option<ContextCompressionConfig>,
    pub agent_compression: Option<CompressionConfig>,
    pub capsule_state: Option<StateCapsule>,
    /// Active PlanFrame summary, used as a relevance lens by the capsule
    /// strategy. `None` when the session has no plan or no active plan.
    /// When set, the capsule strategy prepends an "Active Plan (...)"
    /// block to its delta-extraction prompt so the LLM knows which
    /// decisions, artifacts, and identifiers are plan-advancing (and
    /// which detours can be compressed more aggressively).
    pub plan_anchor: Option<PlanFrameSummary>,
}

impl GovernorContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        history: Vec<Message>,
        tools: Vec<ToolDefinition>,
        breakdown: PromptBudgetBreakdown,
        effective_limit: usize,
        turn_number: u64,
        session_id: String,
        compression_metadata: Option<CompressionMetadata>,
        budget_config: PromptBudgetConfig,
        compression_config: Option<ContextCompressionConfig>,
        agent_compression: Option<CompressionConfig>,
        plan_anchor: Option<PlanFrameSummary>,
    ) -> Self {
        Self {
            history,
            tools,
            breakdown,
            effective_limit,
            turn_number,
            session_id,
            compression_metadata,
            budget_config,
            compression_config,
            agent_compression,
            capsule_state: None,
            plan_anchor,
        }
    }
}

/// Outcome of a single reduction attempt.
pub enum ReductionOutcome {
    /// Strategy resolved the budget issue.
    Resolved {
        tokens_after: usize,
    },
    /// Strategy was insufficient; remaining tokens still exceed limit.
    Insufficient {
        tokens_remaining: usize,
    },
}

/// Result of the full governor pipeline.
pub enum GovernorResult {
    /// Context is within budget without any action.
    WithinBudget,
    /// One or more strategies brought it within budget.
    Recovered {
        actions_taken: Vec<GovernorAction>,
    },
    /// All strategies exhausted; overflow is terminal.
    Overflow(crate::runtime::context_governor::error::ContextOverflowDiagnostic),
}

/// A pluggable context reduction strategy.
#[async_trait::async_trait]
pub trait ReductionStrategy: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Attempt to reduce context usage.
    async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, Role, ToolDefinition};
    use crate::runtime::prompt_budget::PromptBudgetBreakdown;
    use autonoetic_types::config::PromptBudgetConfig;

    /// A strategy that always resolves (for testing pipeline short-circuit).
    pub struct AlwaysResolveStrategy;

    #[async_trait::async_trait]
    impl ReductionStrategy for AlwaysResolveStrategy {
        fn name(&self) -> &'static str {
            "always_resolve"
        }
        async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome> {
            ctx.history.push(Message::user("always_resolve"));
            Ok(ReductionOutcome::Resolved { tokens_after: 1 })
        }
    }

    /// A strategy that always returns insufficient (for testing pipeline fallthrough).
    pub struct AlwaysInsufficientStrategy;

    #[async_trait::async_trait]
    impl ReductionStrategy for AlwaysInsufficientStrategy {
        fn name(&self) -> &'static str {
            "always_insufficient"
        }
        async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome> {
            Ok(ReductionOutcome::Insufficient { tokens_remaining: ctx.breakdown.total_tokens })
        }
    }

    fn make_context(total: usize, limit: usize) -> GovernorContext {
        let breakdown = PromptBudgetBreakdown {
            system_prompt_tokens: 10,
            conversation_tokens: total.saturating_sub(10),
            tool_count: 0,
            tool_definition_tokens: 0,
            total_tokens: total,
            context_window: Some(limit),
            utilization_pct: Some((total as f64 / limit as f64) * 100.0),
        };
        GovernorContext {
            history: vec![],
            tools: vec![],
            breakdown,
            effective_limit: limit,
            turn_number: 1,
            session_id: "test".to_string(),
            compression_metadata: None,
            budget_config: PromptBudgetConfig::default(),
            compression_config: None,
            agent_compression: None,
            capsule_state: None,
            plan_anchor: None,
        }
    }

    #[tokio::test]
    async fn test_within_budget_returns_immediately() {
        let ctx = make_context(50, 100);
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            Box::new(AlwaysResolveStrategy),
        ];
        let governor = crate::runtime::context_governor::ContextGovernor::with_strategies(strategies);
        let mut ctx = ctx;
        let result = governor.govern(&mut ctx).await.unwrap();
        assert!(matches!(result, GovernorResult::WithinBudget));
    }

    #[tokio::test]
    async fn test_first_resolve_skips_later_strategies() {
        let ctx = make_context(150, 100);
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            Box::new(AlwaysResolveStrategy),
            Box::new(AlwaysInsufficientStrategy), // should NOT be reached
        ];
        let governor = crate::runtime::context_governor::ContextGovernor::with_strategies(strategies);
        let mut ctx = ctx;
        let result = governor.govern(&mut ctx).await.unwrap();
        match result {
            GovernorResult::Recovered { actions_taken } => {
                assert_eq!(actions_taken.len(), 1);
                assert_eq!(actions_taken[0].strategy, "always_resolve");
            }
            other => panic!("Expected Recovered, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[tokio::test]
    async fn test_all_exhausted_yields_overflow() {
        let ctx = make_context(200, 100);
        let strategies: Vec<Box<dyn ReductionStrategy>> = vec![
            Box::new(AlwaysInsufficientStrategy),
            Box::new(AlwaysInsufficientStrategy),
        ];
        let governor = crate::runtime::context_governor::ContextGovernor::with_strategies(strategies);
        let mut ctx = ctx;
        let result = governor.govern(&mut ctx).await.unwrap();
        match result {
            GovernorResult::Overflow(diag) => {
                assert_eq!(diag.actions_attempted.len(), 2);
                assert!(diag.budget_snapshot.estimated_input > 0);
            }
            other => panic!("Expected Overflow, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
