//! Budget and cost tracking for agent execution.
//!
//! Contains prompt budget enforcement, context window resolution,
//! cost catalog preflight checks, and related utilities.

use crate::llm::{Message, StopReason, ToolDefinition};
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
/// configured warning threshold. Called unconditionally from the turn-prep
/// path so the event fires under both the strict-governor and legacy budget
/// pipelines.
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

/// Apply prompt budget enforcement based on the configured strategy.
///
/// Returns potentially modified tools and history after enforcement actions.
pub(crate) fn apply_prompt_budget(
    tools: Vec<ToolDefinition>,
    history: Vec<Message>,
    breakdown: &crate::runtime::prompt_budget::PromptBudgetBreakdown,
    config: Option<&GatewayConfig>,
    _session_id: &str,
    _turn_id: &str,
    tracer: &mut SessionTracer,
) -> anyhow::Result<(Vec<ToolDefinition>, Vec<Message>)> {
    let Some(config) = config else {
        return Ok((tools, history));
    };
    let budget_config = &config.prompt_budget;
    let action = &budget_config.on_exceeded;

    let effective_limit = breakdown
        .context_window
        .map(|cw| cw.saturating_sub(budget_config.margin_tokens))
        .unwrap_or(usize::MAX);

    let current_total = breakdown.total_tokens;
    let within_total_budget = current_total <= effective_limit;

    // Check per-section caps. These apply regardless of whether the total
    // budget is satisfied — a section cap is a hard constraint independent
    // of the overall context window.
    let section_cap_violation = {
        let sys_exceeded = budget_config.system_prompt_max_tokens > 0
            && breakdown.system_prompt_tokens > budget_config.system_prompt_max_tokens;
        let tool_exceeded = budget_config.tool_definitions_max_tokens > 0
            && breakdown.tool_definition_tokens > budget_config.tool_definitions_max_tokens;
        sys_exceeded || tool_exceeded
    };

    if !section_cap_violation && within_total_budget {
        return Ok((tools, history));
    }

    let _ = tracer.log_event(
        "agent.process",
        "prompt_budget_enforcement",
        autonoetic_types::causal_chain::EntryStatus::Success,
        Some(serde_json::json!({
            "action": format!("{action:?}"),
            "current_total": current_total,
            "effective_limit": effective_limit,
            "over_by": current_total.saturating_sub(effective_limit),
            "section_cap_violation": section_cap_violation,
        })),
    );

    let strategy = crate::runtime::prompt_budget::enforcement_strategy(*action);
    let result = strategy.enforce(tools, history, breakdown, effective_limit, budget_config)?;
    Ok((result.tools, result.history))
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
    use crate::llm::{CompletionResponse, StopReason, TokenUsage, ToolCall};
    use autonoetic_types::config::GatewayConfig;
    use tempfile::tempdir;

    #[test]
    fn test_apply_prompt_budget_warn_passes_through() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 50,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 180,
            context_window: Some(128_000),
            utilization_pct: Some(0.14),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded = autonoetic_types::config::PromptBudgetAction::Warn;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, result_history) = apply_prompt_budget(
            tools.clone(),
            history.clone(),
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("warn should not fail");

        assert_eq!(result_tools.len(), tools.len());
        assert_eq!(result_history.len(), history.len());
    }

    #[test]
    fn test_apply_prompt_budget_demote_tools_removes_specialized() {
        let tools = vec![
            ToolDefinition {
                name: "content_write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent_spawn".to_string(),
                description: "Spawn agent".to_string(),
                input_schema: serde_json::json!({}),
            },
        ];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        // Use breakdown values consistent with the actual tool definitions.
        // estimate_tool_definition for each tool ≈ 37-39 tokens, so 3 tools ≈ 117.
        // total = 100 (system) + 12 (conv) + 117 (tools) = 229
        // After demotion (remove web.search): 2 tools ≈ 78, total ≈ 190
        // Set context_window = 200 so effective_limit = 200, total 229 > 200 triggers demotion,
        // and filtered total ~190 < 200 succeeds.
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 12,
            tool_count: 3,
            tool_definition_tokens: 117,
            total_tokens: 229,
            context_window: Some(200),
            utilization_pct: Some(114.5),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::DemoteTools;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, result_history) = apply_prompt_budget(
            tools,
            history.clone(),
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("demote tools should not fail");

        assert_eq!(result_tools.len(), 2);
        assert!(result_tools.iter().any(|t| t.name == "content_write"));
        assert!(result_tools.iter().any(|t| t.name == "agent_spawn"));
        assert!(!result_tools.iter().any(|t| t.name == "web_search"));
        assert_eq!(result_history.len(), history.len());
    }

    #[test]
    fn test_apply_prompt_budget_fail_returns_error() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 50,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 180,
            context_window: Some(100),
            utilization_pct: Some(180.0),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded = autonoetic_types::config::PromptBudgetAction::Fail;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let result = apply_prompt_budget(
            tools,
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        );

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Prompt budget exceeded"));
    }

    #[test]
    fn test_apply_prompt_budget_trim_history_removes_oldest_messages() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let long_content = "x".repeat(200);
        let history = vec![
            Message::system("System"),
            Message::user(long_content.clone()),
            Message::assistant(long_content.clone()),
            Message::user(long_content.clone()),
            Message::assistant(long_content.clone()),
            Message::user("Last turn".to_string()),
            Message::assistant("Last reply".to_string()),
        ];
        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 50,
            conversation_tokens: 300,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 380,
            context_window: Some(200),
            utilization_pct: Some(190.0),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::TrimHistory;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, result_history) = apply_prompt_budget(
            tools.clone(),
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("trim history should not fail");

        assert_eq!(result_tools.len(), tools.len());
        assert!(result_history.len() < 7);
        assert!(result_history
            .iter()
            .any(|m| m.role == crate::llm::Role::System));
    }

    #[test]
    fn test_apply_prompt_budget_trim_history_preserves_tool_call_groups() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let long_content = "x".repeat(200);

        // Build history with tool-call exchanges that must stay together:
        // [user, assistant+tool_calls(id="tc1"), tool_result(tc1), user, assistant+tool_calls(id="tc2"), tool_result(tc2), user_final]
        let mut assistant_with_tc1 = Message::assistant(long_content.clone());
        assistant_with_tc1.tool_calls = vec![ToolCall {
            id: "tc1".to_string(),
            name: "content_write".to_string(),
            arguments: "{}".to_string(),
        }];

        let mut assistant_with_tc2 = Message::assistant(long_content.clone());
        assistant_with_tc2.tool_calls = vec![ToolCall {
            id: "tc2".to_string(),
            name: "content_write".to_string(),
            arguments: "{}".to_string(),
        }];

        let history = vec![
            Message::system("System prompt".to_string()),
            Message::user(long_content.clone()),
            assistant_with_tc1,
            Message::tool_result("tc1", "content_write", "ok".to_string()),
            Message::user(long_content.clone()),
            assistant_with_tc2,
            Message::tool_result("tc2", "content_write", "ok".to_string()),
            Message::user("Final question".to_string()),
            Message::assistant("Final reply".to_string()),
        ];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 50,
            conversation_tokens: 1200,
            tool_count: 1,
            tool_definition_tokens: 30,
            total_tokens: 1280,
            context_window: Some(300),
            utilization_pct: Some(426.0),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::TrimHistory;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (_result_tools, result_history) = apply_prompt_budget(
            tools.clone(),
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("trim history should not fail");

        // Verify system message is preserved
        assert!(result_history
            .iter()
            .any(|m| m.role == crate::llm::Role::System));

        // Verify no orphaned tool results: every tool result must have a preceding
        // assistant message with a matching tool call ID
        for msg in &result_history {
            if msg.role == crate::llm::Role::Tool {
                let tc_id = msg
                    .tool_call_id
                    .as_ref()
                    .expect("tool result must have call id");
                let has_matching_assistant = result_history.iter().any(|m| {
                    m.role == crate::llm::Role::Assistant
                        && m.tool_calls.iter().any(|tc| &tc.id == tc_id)
                });
                assert!(
                    has_matching_assistant,
                    "Tool result for '{}' has no matching assistant tool call — group was split",
                    tc_id
                );
            }
        }
    }

    #[test]
    fn test_apply_prompt_budget_section_cap_tool_definitions_triggers_demote_tools() {
        let tools = vec![
            ToolDefinition {
                name: "content_write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent_spawn".to_string(),
                description: "Spawn agent".to_string(),
                input_schema: serde_json::json!({}),
            },
        ];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 100,
            conversation_tokens: 12,
            tool_count: 3,
            tool_definition_tokens: 117,
            total_tokens: 229,
            context_window: Some(10000),
            utilization_pct: Some(2.3),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::DemoteTools;
        config.prompt_budget.tool_definitions_max_tokens = 100;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let (result_tools, _result_history) = apply_prompt_budget(
            tools,
            history.clone(),
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        )
        .expect("demote tools should succeed for section-cap violation");

        assert_eq!(result_tools.len(), 2);
        assert!(!result_tools.iter().any(|t| t.name == "web_search"));
    }

    #[test]
    fn test_apply_prompt_budget_section_cap_system_prompt_fails_for_trim_history() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 500,
            conversation_tokens: 12,
            tool_count: 1,
            tool_definition_tokens: 40,
            total_tokens: 562,
            context_window: Some(10000),
            utilization_pct: Some(5.6),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded =
            autonoetic_types::config::PromptBudgetAction::TrimHistory;
        config.prompt_budget.system_prompt_max_tokens = 200;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let result = apply_prompt_budget(
            tools,
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        );

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("System prompt exceeds configured limit"));
    }

    #[test]
    fn test_enforcement_strategy_factory() {
        use crate::runtime::prompt_budget::enforcement_strategy;
        use autonoetic_types::config::PromptBudgetAction;

        assert_eq!(
            enforcement_strategy(PromptBudgetAction::Warn).name(),
            "warn"
        );
        assert_eq!(
            enforcement_strategy(PromptBudgetAction::TrimHistory).name(),
            "trim_history"
        );
        assert_eq!(
            enforcement_strategy(PromptBudgetAction::DemoteTools).name(),
            "demote_tools"
        );
        assert_eq!(
            enforcement_strategy(PromptBudgetAction::Fail).name(),
            "fail"
        );
    }

    #[test]
    fn test_apply_prompt_budget_fail_on_section_cap_system_prompt() {
        // When on_exceeded = Fail and only system_prompt_max_tokens is violated
        // (total is under effective_limit), the error should mention the system
        // prompt cap specifically, not the generic "prompt budget exceeded".
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({}),
        }];
        let history = vec![Message::user("Hello"), Message::assistant("Hi")];

        let breakdown = crate::runtime::prompt_budget::PromptBudgetBreakdown {
            system_prompt_tokens: 500,
            conversation_tokens: 12,
            tool_count: 1,
            tool_definition_tokens: 40,
            total_tokens: 562,
            context_window: Some(10000),
            utilization_pct: Some(5.6),
        };
        let mut config = GatewayConfig::default();
        config.prompt_budget.on_exceeded = autonoetic_types::config::PromptBudgetAction::Fail;
        config.prompt_budget.system_prompt_max_tokens = 200;
        config.prompt_budget.margin_tokens = 0;

        let temp = tempdir().expect("tempdir should create");
        let mut tracer = SessionTracer::new(temp.path(), "test-agent", "test-session")
            .expect("tracer should create");
        let result = apply_prompt_budget(
            tools,
            history,
            &breakdown,
            Some(&config),
            "s1",
            "t1",
            &mut tracer,
        );

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("System prompt exceeds configured limit"),
            "Expected section-cap error, got: {}",
            err
        );
    }

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
