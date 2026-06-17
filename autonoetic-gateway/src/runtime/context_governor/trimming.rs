use crate::llm::{Message, ToolDefinition};
use crate::runtime::context_governor::strategies::{GovernorContext, ReductionOutcome};
use crate::runtime::prompt_budget::{estimate_message_tokens, estimate_tool_definition, BudgetEnforcementStrategy};
use async_trait::async_trait;

fn recompute_total(system_tokens: usize, history: &[Message], tools: &[ToolDefinition]) -> usize {
    let conv: usize = history.iter()
        .filter(|m| !matches!(m.role, crate::llm::Role::System))
        .map(|m| estimate_message_tokens(m))
        .sum();
    let tool_tokens: usize = tools.iter().map(estimate_tool_definition).sum();
    system_tokens + conv + tool_tokens
}

/// Drop oldest message groups to fit within budget.
pub struct TrimHistoryStrategy;

#[async_trait]
impl super::ReductionStrategy for TrimHistoryStrategy {
    fn name(&self) -> &'static str {
        "trim_history"
    }

    async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome> {
        let system_prompt_tokens = ctx.breakdown.system_prompt_tokens;
        let strategy = crate::runtime::prompt_budget::TrimHistoryStrategy;
        let result = strategy.enforce(
            ctx.tools.clone(),
            ctx.history.clone(),
            &ctx.breakdown,
            ctx.effective_limit,
            &ctx.budget_config,
        )?;

        let new_total = recompute_total(system_prompt_tokens, &result.history, &result.tools);
        if new_total > ctx.effective_limit {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: new_total,
            });
        }

        ctx.tools = result.tools;
        ctx.history = result.history;
        ctx.breakdown.total_tokens = new_total;
        Ok(ReductionOutcome::Resolved {
            tokens_after: new_total,
        })
    }
}
