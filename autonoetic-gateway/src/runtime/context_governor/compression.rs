use crate::runtime::compression::{self};
use crate::runtime::context_governor::strategies::{
    GovernorContext, ReductionOutcome,
};
use autonoetic_types::config::LlmPreset;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;

/// LLM-based summarization strategy. Wraps existing `compress_context()`.
pub struct CompressionStrategy {
    http_client: reqwest::Client,
    presets: HashMap<String, LlmPreset>,
    gateway_dir: Option<PathBuf>,
}

impl CompressionStrategy {
    pub fn new(
        http_client: reqwest::Client,
        presets: HashMap<String, LlmPreset>,
    ) -> Self {
        Self {
            http_client,
            presets,
            gateway_dir: None,
        }
    }

    pub fn with_gateway_dir(mut self, dir: PathBuf) -> Self {
        self.gateway_dir = Some(dir);
        self
    }
}

#[async_trait]
impl super::ReductionStrategy for CompressionStrategy {
    fn name(&self) -> &'static str {
        "compression"
    }

    async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome> {
        let Some(ref cfg) = ctx.compression_config else {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        };
        if !cfg.enabled {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        }

        let result = compression::compress_context(
            ctx.history.clone(),
            ctx.breakdown.context_window,
            cfg,
            ctx.agent_compression.as_ref(),
            &self.presets,
            &self.http_client,
            &ctx.session_id,
            ctx.turn_number,
            ctx.compression_metadata.as_ref(),
        )
        .await?;

        if !result.compressed {
            return Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            });
        }

        let mut new_meta = result.metadata;

        // Persist original history to content store for audit
        if let Some(ref dir) = self.gateway_dir {
            if let Ok(Some(handle)) = compression::persist_compressed_context(
                dir,
                &ctx.session_id,
                &result.original_history,
                &new_meta,
            ) {
                new_meta.compressed_context_handle = Some(handle);
            }
        }

        let conv_text: String = result
            .history
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let tokens_after = crate::runtime::prompt_budget::estimate_tokens(&conv_text);
        let new_total = ctx
            .breakdown
            .total_tokens
            .saturating_sub(ctx.breakdown.conversation_tokens)
            .saturating_add(tokens_after);

        ctx.history = result.history;
        ctx.compression_metadata = Some(new_meta);
        // Update breakdown total to reflect compressed history
        ctx.breakdown.conversation_tokens = tokens_after;
        ctx.breakdown.total_tokens = new_total;

        let still_over = ctx.breakdown.total_tokens > ctx.effective_limit;
        if still_over {
            Ok(ReductionOutcome::Insufficient {
                tokens_remaining: ctx.breakdown.total_tokens,
            })
        } else {
            Ok(ReductionOutcome::Resolved {
                tokens_after: ctx.breakdown.total_tokens,
            })
        }
    }
}
