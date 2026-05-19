//! Budget and cost tracking for agent execution.
//!
//! Contains pre-LLM context-pressure observability, context window resolution,
//! and cost catalog preflight checks. Budget enforcement and reduction live
//! in `runtime::context_governor`.

use crate::llm::StopReason;
use crate::runtime::lifecycle::AgentExecutor;
use crate::runtime::openrouter_catalog::OpenRouterCatalog;
use crate::runtime::session_tracer::SessionTracer;
use autonoetic_types::agent::AgentManifest;
use autonoetic_types::config::GatewayConfig;
use std::sync::Arc;

pub(crate) const LLM_OTHER_EMPTY_RETRY_ENV: &str = "AUTONOETIC_LLM_OTHER_EMPTY_RETRIES";
pub(crate) const LLM_OTHER_EMPTY_RETRY_DEFAULT: usize = 1;

pub(crate) fn max_other_empty_retries() -> usize {
    std::env::var(LLM_OTHER_EMPTY_RETRY_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(LLM_OTHER_EMPTY_RETRY_DEFAULT)
}

pub(crate) fn is_retryable_empty_other_response(response: &crate::llm::CompletionResponse) -> bool {
    matches!(&response.stop_reason, StopReason::Other(s) if s.trim().is_empty())
        && response.tool_calls.is_empty()
        && response.text.trim().is_empty()
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

pub(crate) fn resolve_context_window_tokens(manifest: &AgentManifest) -> Option<u32> {
    if let Some(cfg) = &manifest.llm_config {
        if let Some(w) = cfg.context_window_tokens {
            return Some(w);
        }
    }
    std::env::var("AUTONOETIC_LLM_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Manifest/env first; if still unknown and provider is OpenRouter, use the public models API cache.
pub(crate) async fn resolve_context_window_for_run(
    manifest: &AgentManifest,
    model: &str,
    catalog: Option<&Arc<OpenRouterCatalog>>,
) -> Option<u32> {
    if let Some(w) = resolve_context_window_tokens(manifest) {
        return Some(w);
    }
    let use_openrouter = manifest
        .llm_config
        .as_ref()
        .map(|c| c.provider.eq_ignore_ascii_case("openrouter"))
        .unwrap_or(false);
    if !use_openrouter {
        return None;
    }
    match catalog {
        Some(cat) => cat.context_length_for_model(model).await,
        None => None,
    }
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

        let mode = crate::fail_mode::lookup_fail_mode("R-6.5")
            .map(|m| m.to_string())
            .unwrap_or_else(|| "refuse-session-start".to_string());
        let Some(catalog) = self.openrouter_catalog.as_ref() else {
            anyhow::bail!(
                "Session start refused: cost-budget enforcement requires price metadata but \
                 catalog is unavailable (R-6.5, R++10: fail-mode={}). \
                 Add capability 'budget.no_price_available.allow' to override intentionally.",
                mode
            );
        };
        if catalog.estimate_cost_usd(model_id, 1, 1).await.is_none() {
            anyhow::bail!(
                "Session start refused: cost-budget enforcement requires price metadata for model '{}' \
                 but catalog is unavailable (R-6.5, R++10: fail-mode={}). \
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
            stop_reason: StopReason::Other(String::new()),
            usage: TokenUsage::default(),
        };
        assert!(is_retryable_empty_other_response(&retryable));

        let not_retryable = CompletionResponse {
            text: "has text".to_string(),
            tool_calls: vec![],
            reasoning_content: None,
            stop_reason: StopReason::Other(String::new()),
            usage: TokenUsage::default(),
        };
        assert!(!is_retryable_empty_other_response(&not_retryable));
    }
}
