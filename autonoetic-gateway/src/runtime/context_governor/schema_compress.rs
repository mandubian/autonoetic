use crate::llm::ToolDefinition;
use crate::runtime::context_governor::strategies::{GovernorContext, ReductionOutcome};
use crate::runtime::prompt_budget::{compress_tool_definitions, estimate_tool_definition};
use async_trait::async_trait;

fn tool_token_total(tools: &[ToolDefinition]) -> usize {
    tools.iter().map(estimate_tool_definition).sum()
}

/// Strip tool JSON schemas to `{}` after turn 0.
/// When `force_on_turn_0` is true, also compresses on the first turn
/// (used by the aggressive overflow-recovery pipeline).
pub struct ToolSchemaCompressionStrategy {
    pub force_on_turn_0: bool,
}

impl ToolSchemaCompressionStrategy {
    pub fn new() -> Self {
        Self { force_on_turn_0: false }
    }

    pub fn forced() -> Self {
        Self { force_on_turn_0: true }
    }
}

impl Default for ToolSchemaCompressionStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl super::ReductionStrategy for ToolSchemaCompressionStrategy {
    fn name(&self) -> &'static str {
        "tool_schema_compression"
    }

    async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome> {
        if ctx.turn_number == 0 && !self.force_on_turn_0 {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        }

        let before_tool_tokens = tool_token_total(&ctx.tools);
        let compressed = compress_tool_definitions(
            ctx.tools.clone(),
            // When forcing compression on turn 0, pass a non-zero turn
            // because the helper skips turn_number == 0.
            if self.force_on_turn_0 && ctx.turn_number == 0 { 1 } else { ctx.turn_number as usize },
        );
        let after_tool_tokens = tool_token_total(&compressed);
        let saved = before_tool_tokens.saturating_sub(after_tool_tokens);
        let new_total = ctx.breakdown.total_tokens.saturating_sub(saved);

        if new_total > ctx.effective_limit {
            ctx.tools = compressed;
            ctx.breakdown.total_tokens = new_total;
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: new_total,
            });
        }

        ctx.tools = compressed;
        ctx.breakdown.total_tokens = new_total;
        Ok(ReductionOutcome::Resolved {
            tokens_after: new_total,
        })
    }
}
