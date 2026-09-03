//! Budget and cost tracking for agent execution.
//!
//! Contains pre-LLM context-pressure observability, context window resolution,
//! and cost catalog preflight checks. Budget enforcement and reduction live
//! in `runtime::context_governor`.

use crate::llm::StopReason;
use crate::runtime::lifecycle::AgentExecutor;
use crate::runtime::session_tracer::SessionTracer;
use autonoetic_types::config::GatewayConfig;

pub(crate) const LLM_OTHER_EMPTY_RETRY_ENV: &str = "AUTONOETIC_LLM_OTHER_EMPTY_RETRIES";
pub(crate) const LLM_OTHER_EMPTY_RETRY_DEFAULT: usize = 1;

pub(crate) fn max_other_empty_retries() -> usize {
    std::env::var(LLM_OTHER_EMPTY_RETRY_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(LLM_OTHER_EMPTY_RETRY_DEFAULT)
}

pub(crate) fn is_retryable_empty_other_response(response: &crate::llm::CompletionResponse) -> bool {
    let empty_payload = response.tool_calls.is_empty() && response.text.trim().is_empty();
    if !empty_payload {
        return false;
    }
    match &response.stop_reason {
        // Provider said stop with an empty reason — always suspect.
        StopReason::Other(s) => s.trim().is_empty(),
        // A legitimate end-of-turn always carries the final assistant message
        // (or at least one output token of hidden reasoning). Zero output
        // tokens means the provider failed, not that the model chose to stop —
        // e.g. ninfer returning empty completions under load, which used to
        // end the turn silently and idle the session until an operator nudge.
        StopReason::EndTurn => response.usage.output_tokens == 0,
        // `ToolUse` with an empty tool set is contradictory provider output —
        // treat it like the other zero-progress failures and retry.
        StopReason::ToolUse => true,
        StopReason::MaxTokens | StopReason::StopSequence => false,
    }
}

/// Emit a `context_pressure_high` causal event when utilization crosses the
/// configured warning threshold. Called from the turn-prep path so the event
/// fires regardless of whether the governor needs to reduce.
pub(crate) fn emit_context_pressure_high_if_warranted(
    breakdown: &crate::runtime::prompt_budget::PromptBudgetBreakdown,
    config: Option<&GatewayConfig>,
    tracer: &mut SessionTracer,
) {
    let Some(config) = config else { return };
    let budget_config = &config.prompt_budget;
    let Some(pct) = breakdown.utilization_pct else {
        return;
    };
    if pct < budget_config.warn_at_pct {
        return;
    }
    let effective_limit = breakdown
        .context_window
        .map(|cw| cw.saturating_sub(budget_config.margin_tokens))
        .unwrap_or(usize::MAX);
    tracing::warn!(
        target: "autonoetic::prompt_budget",
        utilization_pct = pct,
        warn_threshold = budget_config.warn_at_pct,
        total_tokens = breakdown.total_tokens,
        "Prompt budget approaching limit"
    );
    let _ = tracer.log_event(
        "agent.process",
        "context_pressure_high",
        autonoetic_types::causal_chain::EntryStatus::Success,
        Some(serde_json::json!({
            "utilization_pct": pct,
            "total_tokens": breakdown.total_tokens,
            "effective_limit": effective_limit,
            "margin_tokens": budget_config.margin_tokens,
            "context_window": breakdown.context_window,
            "warning_threshold_pct": budget_config.warn_at_pct,
        })),
    );
}

/// Parsed `AUTONOETIC_LLM_CONTEXT_WINDOW` override, cached for the process
/// lifetime (#591). The override is fixed at startup, so we read and parse it
/// once instead of on every context-window resolution. Shared by the context
/// governor resolver to avoid a second per-call env read.
pub(crate) fn llm_context_window_env_tokens() -> Option<u32> {
    static CACHED: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("AUTONOETIC_LLM_CONTEXT_WINDOW")
            .ok()
            .and_then(|s| s.trim().parse().ok())
    })
}

/// Maps provider prompt (`input`) token count to % of a declared context window.
pub(crate) fn input_tokens_as_context_pct(input_tokens: u64, context_window: Option<u32>) -> Option<f32> {
    let w = f64::from(context_window?);
    if w <= 0.0 {
        return None;
    }
    let pct = (input_tokens as f64 / w) * 100.0;
    Some(pct.min(9999.0) as f32)
}

impl AgentExecutor {
    /// Best-effort budget snapshot for the attestation block. Pulls usage
    /// from the per-session registry and pairs it with the configured
    /// limit (when one exists). Returns an empty list when budgets are
    /// disabled or counters have not been observed yet for this session.
    pub(crate) fn snapshot_budget_meters(&self) -> Vec<crate::runtime::state_attestation::BudgetMeter> {
        use crate::runtime::state_attestation::BudgetMeter;
        let mut meters = Vec::new();
        let session_id = match self.session_id.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => return meters,
        };
        let Some(reg) = self.session_budget.as_ref() else {
            return meters;
        };
        let Some(cfg) = self.config.as_ref() else {
            return meters;
        };
        let limits = &cfg.session_budget;
        if let Some((rounds, tokens, cost)) = reg.snapshot_counters(session_id) {
            meters.push(BudgetMeter {
                name: "llm_rounds".to_string(),
                used: rounds as f64,
                limit: limits.max_llm_rounds.map(|x| x as f64),
            });
            meters.push(BudgetMeter {
                name: "llm_tokens".to_string(),
                used: tokens as f64,
                limit: limits.max_llm_tokens.map(|x| x as f64),
            });
            meters.push(BudgetMeter {
                name: "session_price_usd".to_string(),
                used: cost,
                limit: limits.max_session_price_usd,
            });
        }
        meters
    }

    pub(crate) async fn enforce_cost_catalog_preflight(
        &self,
        model_id: &str,
        allow_unpriced_budget: bool,
    ) -> anyhow::Result<()> {
        if allow_unpriced_budget {
            return Ok(());
        }
        let Some(cfg) = self.config.as_ref() else {
            return Ok(());
        };
        let session_price_cap_enabled = cfg
            .session_budget
            .max_session_price_usd
            .is_some_and(|v| v >= 0.0);
        let root_price_cap_enabled = cfg
            .root_session_budget
            .max_session_price_usd
            .is_some_and(|v| v >= 0.0);
        if !session_price_cap_enabled && !root_price_cap_enabled {
            return Ok(());
        }

        let mode = crate::fail_mode::lookup_fail_mode("P-6.5")
            .map(|m| m.to_string())
            .unwrap_or_else(|| "refuse-session-start".to_string());
        let Some(catalog) = self.openrouter_catalog.as_ref() else {
            anyhow::bail!(
                "Session start refused: cost-budget enforcement requires price metadata but \
                 catalog is unavailable (P-6.5, I-11: fail-mode={}). \
                 Add capability 'budget.no_price_available.allow' to override intentionally.",
                mode
            );
        };
        if catalog.estimate_cost_usd(model_id, 1, 1).await.is_none() {
            anyhow::bail!(
                "Session start refused: cost-budget enforcement requires price metadata for model '{}' \
                 but catalog is unavailable (P-6.5, I-11: fail-mode={}). \
                 Add capability 'budget.no_price_available.allow' to override intentionally.",
                model_id,
                mode
            );
        }
        Ok(())
    }

    pub(crate) fn root_session_id_opt(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .map(crate::runtime::content_store::root_session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CompletionResponse, StopReason, TokenUsage};

    #[test]
    fn test_is_retryable_empty_other_response() {
        let retryable = CompletionResponse {
            text: String::new(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::Other(String::new()),
            usage: TokenUsage::default(),
        };
        assert!(is_retryable_empty_other_response(&retryable));

        let not_retryable = CompletionResponse {
            text: "has text".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::Other(String::new()),
            usage: TokenUsage::default(),
        };
        assert!(!is_retryable_empty_other_response(&not_retryable));
    }

    #[test]
    fn zero_output_end_turn_is_retryable_provider_failure() {
        // What ninfer returned 3× on 2026-09-03: EndTurn with zero usage —
        // a provider failure that used to end the turn silently.
        let empty_end_turn = CompletionResponse {
            text: String::new(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        assert!(is_retryable_empty_other_response(&empty_end_turn));

        // A real end-of-turn carries the final message (or reasoning tokens).
        let normal_end_turn = CompletionResponse {
            text: "final answer".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 5,
                reasoning_tokens: 4,
                cached_tokens: 0,
            },
        };
        assert!(!is_retryable_empty_other_response(&normal_end_turn));

        // Tool calls take precedence even at zero reported usage.
        let with_tool_call = CompletionResponse {
            text: String::new(),
            tool_calls: vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "resolve".into(),
                arguments: "{}".into(),
            }],
            reasoning_content: None,
            reasoning_details: None,
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        };
        assert!(!is_retryable_empty_other_response(&with_tool_call));
    }
}
