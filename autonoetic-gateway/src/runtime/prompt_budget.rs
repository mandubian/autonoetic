//! Prompt Budget Transparency.
//!
//! Provides observability into what consumes the context window budget
//! before each LLM call, and tool tiering for dynamic tool filtering.

use crate::llm::{Message, ToolDefinition};
use serde::Serialize;
use std::collections::HashSet;

/// Heuristic: ~4 characters per token (works across most models).
const CHARS_PER_TOKEN: f64 = 4.0;

/// Estimated overhead tokens per tool definition (name + description + schema structure).
const TOOL_OVERHEAD_TOKENS: usize = 30;

/// Tool tier for progressive disclosure.
pub type ToolTier = autonoetic_types::agent::ToolTier;

/// Breakdown of the prompt budget before an LLM call.
#[derive(Debug, Clone, Serialize)]
pub struct PromptBudgetBreakdown {
    /// Tokens in the system prompt (foundation + agent instructions).
    pub system_prompt_tokens: usize,
    /// Tokens in the conversation history (all messages except system).
    pub conversation_tokens: usize,
    /// Number of tool definitions included.
    pub tool_count: usize,
    /// Estimated tokens for all tool definitions.
    pub tool_definition_tokens: usize,
    /// Total estimated prompt tokens.
    pub total_tokens: usize,
    /// Context window size for the target model (if known).
    pub context_window: Option<usize>,
    /// Utilization percentage of the context window (0-100).
    pub utilization_pct: Option<f64>,
}

impl PromptBudgetBreakdown {
    /// Compute a budget breakdown from the assembled request components.
    pub fn compute(
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
        context_window: Option<usize>,
    ) -> Self {
        let system_prompt_tokens = estimate_tokens(system_prompt);

        let conversation_tokens: usize = messages
            .iter()
            .filter(|m| m.role != crate::llm::Role::System)
            .map(|m| estimate_tokens(&m.content))
            .sum();

        let tool_count = tools.len();
        let tool_definition_tokens: usize = tools.iter().map(|t| estimate_tool_definition(t)).sum();

        let total_tokens = system_prompt_tokens + conversation_tokens + tool_definition_tokens;

        let utilization_pct = context_window.map(|cw| {
            if cw == 0 {
                0.0
            } else {
                (total_tokens as f64 / cw as f64) * 100.0
            }
        });

        Self {
            system_prompt_tokens,
            conversation_tokens,
            tool_count,
            tool_definition_tokens,
            total_tokens,
            context_window,
            utilization_pct,
        }
    }
}

/// Estimate tokens from a text string using the ~4 chars/token heuristic.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as usize
    }
}

/// Estimate tokens for a single tool definition.
pub fn estimate_tool_definition(tool: &ToolDefinition) -> usize {
    let name_tokens = estimate_tokens(&tool.name);
    let desc_tokens = estimate_tokens(&tool.description);
    let schema_tokens =
        estimate_tokens(&serde_json::to_string(&tool.input_schema).unwrap_or_default());
    name_tokens + desc_tokens + schema_tokens + TOOL_OVERHEAD_TOKENS
}

/// Get the tier for a tool using the declarative registry.
pub fn tool_tier(tool_name: &str) -> ToolTier {
    crate::runtime::tool_tier_registry::tool_tier(tool_name)
}

/// Filter tool definitions by tier.
pub fn filter_tools_by_tier(
    tools: Vec<ToolDefinition>,
    allowed_tiers: &[ToolTier],
) -> Vec<ToolDefinition> {
    if allowed_tiers.is_empty() {
        return tools;
    }
    tools
        .into_iter()
        .filter(|t| allowed_tiers.contains(&tool_tier(&t.name)))
        .collect()
}

/// Whether `tool_name` matches a `tool_discover` pattern (exact name or `prefix*`).
pub fn tool_matches_discovered_pattern(tool_name: &str, pattern: &str) -> bool {
    let p = pattern.trim();
    if p.is_empty() {
        return false;
    }
    if let Some(prefix) = p.strip_suffix('*') {
        tool_name.starts_with(prefix)
    } else {
        tool_name == p
    }
}

/// Cap tool count for the LLM request, dropping lowest-priority tiers first.
///
/// Tools explicitly matched by `discovered_patterns` (from `tool_discover`) are
/// never dropped — the agent asked for them by name.
pub fn cap_tool_definitions_preserving_discovered(
    tools: Vec<ToolDefinition>,
    max_tools: usize,
    discovered_patterns: &HashSet<String>,
) -> Vec<ToolDefinition> {
    if max_tools == 0 || tools.len() <= max_tools {
        return tools;
    }

    let (mut pinned, mut rest): (Vec<ToolDefinition>, Vec<ToolDefinition>) = tools
        .into_iter()
        .partition(|def| {
            discovered_patterns
                .iter()
                .any(|pattern| tool_matches_discovered_pattern(&def.name, pattern))
        });

    let budget = max_tools.saturating_sub(pinned.len());
    if rest.len() > budget {
        let tier_order = |tier: &ToolTier| match tier {
            ToolTier::Core => 0,
            ToolTier::Workflow => 1,
            ToolTier::Specialized => 2,
        };
        rest.sort_by(|a, b| {
            tier_order(&tool_tier(&a.name)).cmp(&tier_order(&tool_tier(&b.name)))
        });
        rest.truncate(budget);
    }

    rest.append(&mut pinned);
    rest
}

/// Compress tool definitions for subsequent turns.
///
/// On turn 0, returns full tool definitions. On subsequent turns,
/// replaces detailed JSON schemas with minimal `{}` schemas since
/// the model already knows the tools from the first turn.
pub fn compress_tool_definitions(
    tools: Vec<ToolDefinition>,
    turn_number: usize,
) -> Vec<ToolDefinition> {
    if turn_number == 0 {
        return tools;
    }
    tools
        .into_iter()
        .map(|t| ToolDefinition {
            name: t.name,
            description: t.description,
            input_schema: serde_json::json!({}),
        })
        .collect()
}

/// Result of applying a prompt budget enforcement strategy.
#[derive(Debug)]
pub struct EnforcementResult {
    pub tools: Vec<ToolDefinition>,
    pub history: Vec<Message>,
    pub was_trimmed: bool,
}

/// Strategy for enforcing prompt budget limits when the total exceeds the effective limit.
pub trait BudgetEnforcementStrategy: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn enforce(
        &self,
        tools: Vec<ToolDefinition>,
        history: Vec<Message>,
        breakdown: &PromptBudgetBreakdown,
        effective_limit: usize,
        budget_config: &autonoetic_types::config::PromptBudgetConfig,
    ) -> anyhow::Result<EnforcementResult>;
}

fn check_section_caps(
    breakdown: &PromptBudgetBreakdown,
    budget_config: &autonoetic_types::config::PromptBudgetConfig,
    can_fix_system: bool,
    can_fix_tools: bool,
) -> anyhow::Result<()> {
    if budget_config.system_prompt_max_tokens > 0
        && breakdown.system_prompt_tokens > budget_config.system_prompt_max_tokens
    {
        if !can_fix_system {
            anyhow::bail!(
                "System prompt exceeds configured limit: {} tokens (limit: {})",
                breakdown.system_prompt_tokens,
                budget_config.system_prompt_max_tokens,
            );
        }
    }
    if budget_config.tool_definitions_max_tokens > 0
        && breakdown.tool_definition_tokens > budget_config.tool_definitions_max_tokens
    {
        if !can_fix_tools {
            anyhow::bail!(
                "Tool definitions exceed configured limit: {} tokens (limit: {})",
                breakdown.tool_definition_tokens,
                budget_config.tool_definitions_max_tokens,
            );
        }
    }
    Ok(())
}

/// Trim history strategy: removes oldest message groups to fit within budget.
#[derive(Debug, Clone, Copy)]
pub struct TrimHistoryStrategy;

impl BudgetEnforcementStrategy for TrimHistoryStrategy {
    fn name(&self) -> &'static str {
        "trim_history"
    }

    fn enforce(
        &self,
        _tools: Vec<ToolDefinition>,
        history: Vec<Message>,
        breakdown: &PromptBudgetBreakdown,
        effective_limit: usize,
        budget_config: &autonoetic_types::config::PromptBudgetConfig,
    ) -> anyhow::Result<EnforcementResult> {
        check_section_caps(breakdown, budget_config, false, false)?;
        let non_system: Vec<_> = history
            .iter()
            .filter(|m| m.role != crate::llm::Role::System)
            .cloned()
            .collect();
        let system: Vec<_> = history
            .iter()
            .filter(|m| m.role == crate::llm::Role::System)
            .cloned()
            .collect();

        let budget_for_conv = effective_limit
            .saturating_sub(breakdown.system_prompt_tokens)
            .saturating_sub(breakdown.tool_definition_tokens);

        let mut groups: Vec<(Vec<Message>, usize)> = Vec::new();
        let mut current_group: Vec<Message> = Vec::new();
        let mut current_group_tokens: usize = 0;
        let mut pending_tool_call_ids: HashSet<String> = HashSet::new();

        for msg in non_system {
            let msg_tokens = estimate_tokens(&msg.content);

            if msg.role == crate::llm::Role::Tool {
                current_group.push(msg);
                current_group_tokens += msg_tokens;
                if let Some(id) = pending_tool_call_ids.iter().next().cloned() {
                    pending_tool_call_ids.remove(&id);
                }
                if pending_tool_call_ids.is_empty() && !current_group.is_empty() {
                    groups.push((std::mem::take(&mut current_group), current_group_tokens));
                    current_group_tokens = 0;
                }
            } else if msg.role == crate::llm::Role::Assistant && !msg.tool_calls.is_empty() {
                pending_tool_call_ids = msg.tool_calls.iter().map(|tc| tc.id.clone()).collect();
                current_group.push(msg);
                current_group_tokens += msg_tokens;
                if pending_tool_call_ids.is_empty() {
                    groups.push((std::mem::take(&mut current_group), current_group_tokens));
                    current_group_tokens = 0;
                }
            } else {
                if !current_group.is_empty() {
                    groups.push((std::mem::take(&mut current_group), current_group_tokens));
                    current_group_tokens = 0;
                }
                groups.push((vec![msg], msg_tokens));
            }
        }
        if !current_group.is_empty() {
            groups.push((current_group, current_group_tokens));
        }

        let total_tokens: usize = groups.iter().map(|(_, t)| *t).sum();
        let mut current_total = total_tokens;

        let min_groups = 2.min(groups.len());

        while current_total > budget_for_conv && groups.len() > min_groups {
            let (_, group_tokens) = groups.remove(0);
            current_total = current_total.saturating_sub(group_tokens);
        }

        if current_total > budget_for_conv {
            anyhow::bail!(
                "Cannot trim history to fit within prompt budget: {} tokens remaining (budget: {}), \
                 hit message floor. Consider increasing the context window \
                 or reducing system prompt/tool definition size.",
                current_total,
                budget_for_conv,
            );
        }

        let mut new_history = system;
        for (group, _) in groups {
            new_history.extend(group);
        }

        tracing::warn!(
            target: "autonoetic::prompt_budget",
            trimmed_messages = history.len() - new_history.len(),
            "Trimmed conversation history to fit within prompt budget"
        );

        Ok(EnforcementResult {
            tools: _tools,
            history: new_history,
            was_trimmed: true,
        })
    }
}

/// Demote tools strategy: removes specialized-tier tools to reduce token usage.
#[derive(Debug, Clone, Copy)]
pub struct DemoteToolsStrategy;

impl BudgetEnforcementStrategy for DemoteToolsStrategy {
    fn name(&self) -> &'static str {
        "demote_tools"
    }

    fn enforce(
        &self,
        tools: Vec<ToolDefinition>,
        history: Vec<Message>,
        breakdown: &PromptBudgetBreakdown,
        effective_limit: usize,
        budget_config: &autonoetic_types::config::PromptBudgetConfig,
    ) -> anyhow::Result<EnforcementResult> {
        check_section_caps(breakdown, budget_config, false, true)?;

        let filtered = filter_tools_by_tier(tools, &[ToolTier::Core, ToolTier::Workflow]);
        let removed_count = breakdown.tool_count - filtered.len();

        let filtered_tool_tokens: usize = filtered.iter().map(estimate_tool_definition).sum();

        if budget_config.tool_definitions_max_tokens > 0
            && filtered_tool_tokens > budget_config.tool_definitions_max_tokens
        {
            anyhow::bail!(
                "Tool definitions still exceed limit after demotion: {} tokens (limit: {}). \
                 Core and workflow tools alone exceed the configured cap.",
                filtered_tool_tokens,
                budget_config.tool_definitions_max_tokens,
            );
        }

        let filtered_total =
            breakdown.total_tokens - breakdown.tool_definition_tokens + filtered_tool_tokens;
        if filtered_total > effective_limit {
            anyhow::bail!(
                "Prompt budget still exceeded after tool demotion: {} tokens (limit: {}). \
                 Removed {} specialized tools but core/workflow tools are still too large.",
                filtered_total,
                effective_limit,
                removed_count,
            );
        }

        tracing::warn!(
            target: "autonoetic::prompt_budget",
            removed_tools = removed_count,
            tool_tokens_before = breakdown.tool_definition_tokens,
            tool_tokens_after = filtered_tool_tokens,
            "Demoted specialized tools to fit within prompt budget"
        );

        Ok(EnforcementResult {
            tools: filtered,
            history,
            was_trimmed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_approximate() {
        let text = "hello world this is a test";
        let tokens = estimate_tokens(text);
        assert!(tokens > 0);
    }

    #[test]
    fn test_tool_tier_core() {
        assert_eq!(tool_tier("content_write"), ToolTier::Core);
        assert_eq!(tool_tier("resolve"), ToolTier::Core);
        assert_eq!(tool_tier("knowledge_store"), ToolTier::Core);
        assert_eq!(tool_tier("sandbox_exec"), ToolTier::Core);
    }

    #[test]
    fn test_tool_tier_workflow() {
        assert_eq!(tool_tier("agent_spawn"), ToolTier::Workflow);
        assert_eq!(tool_tier("workflow_wait"), ToolTier::Workflow);
        assert_eq!(tool_tier("approval_status"), ToolTier::Workflow);
    }

    #[test]
    fn test_tool_tier_specialized() {
        assert_eq!(tool_tier("web_search"), ToolTier::Specialized);
        assert_eq!(tool_tier("web_fetch"), ToolTier::Specialized);
        assert_eq!(tool_tier("promotion_record"), ToolTier::Specialized);
    }

    #[test]
    fn test_filter_tools_by_tier() {
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

        let core_only = filter_tools_by_tier(tools.clone(), &[ToolTier::Core]);
        assert_eq!(core_only.len(), 1);
        assert_eq!(core_only[0].name, "content_write");

        let core_and_workflow =
            filter_tools_by_tier(tools.clone(), &[ToolTier::Core, ToolTier::Workflow]);
        assert_eq!(core_and_workflow.len(), 2);

        let all = filter_tools_by_tier(tools, &[]);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_budget_breakdown_compute() {
        let system = "You are a helpful assistant.";
        let messages = vec![Message::user("Hello"), Message::assistant("Hi there")];
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];

        let breakdown = PromptBudgetBreakdown::compute(system, &messages, &tools, Some(128_000));

        assert!(breakdown.system_prompt_tokens > 0);
        assert!(breakdown.conversation_tokens > 0);
        assert_eq!(breakdown.tool_count, 1);
        assert!(breakdown.tool_definition_tokens > 0);
        assert!(breakdown.total_tokens > 0);
        assert_eq!(breakdown.context_window, Some(128_000));
        assert!(breakdown.utilization_pct.is_some());
        assert!(breakdown.utilization_pct.unwrap() > 0.0);
        assert!(breakdown.utilization_pct.unwrap() < 100.0);
    }

    #[test]
    fn test_compress_tool_definitions_turn_zero_keeps_full() {
        let tools = vec![ToolDefinition {
            name: "content_write".to_string(),
            description: "Write content".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        }];

        let compressed = compress_tool_definitions(tools.clone(), 0);
        assert_eq!(compressed.len(), 1);
        assert_eq!(compressed[0].input_schema["type"], "object");
    }

    #[test]
    fn test_compress_tool_definitions_subsequent_turns_minimal() {
        let tools = vec![
            ToolDefinition {
                name: "content_write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {"name": {"type": "string"}}}),
            },
            ToolDefinition {
                name: "web_search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            },
        ];

        let compressed = compress_tool_definitions(tools, 3);
        assert_eq!(compressed.len(), 2);
        assert_eq!(compressed[0].input_schema, serde_json::json!({}));
        assert_eq!(compressed[1].input_schema, serde_json::json!({}));
        assert_eq!(compressed[0].name, "content_write");
        assert_eq!(compressed[1].name, "web_search");
    }

    #[test]
    fn cap_tool_definitions_preserves_discovered_specialized_tools() {
        let tools: Vec<ToolDefinition> = (0..45)
            .map(|i| ToolDefinition {
                name: format!("core_tool_{i}"),
                description: "core".to_string(),
                input_schema: serde_json::json!({}),
            })
            .chain([ToolDefinition {
                name: "federation.escalate".to_string(),
                description: "Escalate".to_string(),
                input_schema: serde_json::json!({}),
            }])
            .collect();

        let discovered = HashSet::from(["federation.escalate".to_string()]);
        let capped = cap_tool_definitions_preserving_discovered(tools, 40, &discovered);

        assert_eq!(capped.len(), 40);
        assert!(
            capped.iter().any(|t| t.name == "federation.escalate"),
            "discovered tool must survive truncation"
        );
    }

    #[test]
    fn planner_tool_surface_counts_by_tier() {
        use crate::runtime::tool_dispatch::determine_tool_tier_filter;
        use crate::runtime::tools::default_registry;
        use autonoetic_types::agent::{AgentIdentity, AgentManifest, RuntimeDeclaration};
        use autonoetic_types::capability::Capability;

        let manifest = AgentManifest {
            version: "1.0".to_string(),
            runtime: RuntimeDeclaration {
                engine: "autonoetic".to_string(),
                gateway_version: "0.1.0".to_string(),
                sdk_version: "0.1.0".to_string(),
                runtime_type: "stateful".to_string(),
                sandbox: "bubblewrap".to_string(),
                runtime_lock: "runtime.lock".to_string(),
            },
            agent: AgentIdentity {
                id: "planner.default".to_string(),
                name: "Planner".to_string(),
                description: "test".to_string(),
            },
            capabilities: vec![
                Capability::SandboxFunctions {
                    allowed: vec![
                        "knowledge.".to_string(),
                        "agent.".to_string(),
                        "credential.".to_string(),
                    ],
                },
                Capability::CredentialAccess {
                    services: vec!["*".to_string()],
                },
                Capability::AgentSpawn {
                    max_children: 10,
                    max_spawn_depth: 0,
                },
                Capability::SchedulerAccess {
                    patterns: vec!["*".to_string()],
                },
                Capability::WriteAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
                Capability::ReadAccess {
                    scopes: vec!["self.*".to_string(), "skills/*".to_string()],
                },
                Capability::AgentMessage {
                    patterns: vec!["*".to_string()],
                },
            ],
            llm_config: None,
            limits: None,
            background: None,
            disclosure: None,
            io: None,
            middleware: None,
            execution_mode: Default::default(),
            script_entry: None,
            script_input_mode: Default::default(),
            gateway_url: None,
            gateway_token: None,
            allowed_tool_tiers: vec![],
            agentskills_import: None,
            compression: None,
            sandbox_network: autonoetic_types::agent::SandboxNetworkPolicy::default(),
        };

        let registry = default_registry();
        let all_available = registry.available_definitions(&manifest);
        let core_workflow_filter =
            determine_tool_tier_filter(&manifest, Some("session-root"), false, autonoetic_types::agent::SessionState::Normal, false);
        let core_workflow = registry.available_definitions_filtered(&manifest, Some(&core_workflow_filter));
        let all_tiers_filter =
            determine_tool_tier_filter(&manifest, Some("session-root"), false, autonoetic_types::agent::SessionState::Normal, true);
        let all_tiers = registry.available_definitions_filtered(&manifest, Some(&all_tiers_filter));

        eprintln!(
            "planner tool counts: all_available={} core+workflow={} all_tiers_escalated={}",
            all_available.len(),
            core_workflow.len(),
            all_tiers.len()
        );
        let specialized_only: Vec<&str> = all_tiers
            .iter()
            .filter(|d| !core_workflow.iter().any(|c| c.name == d.name))
            .map(|d| d.name.as_str())
            .collect();
        eprintln!(
            "planner specialized-only (need tier escalation): {:?}",
            specialized_only
        );

        // Documented baseline in config-template.yaml — keep in sync when tiers change.
        assert!(
            core_workflow.len() >= 35 && core_workflow.len() <= 45,
            "expected ~40 core+workflow tools for planner, got {}",
            core_workflow.len()
        );
        assert!(
            all_tiers.len() >= 45 && all_tiers.len() <= 55,
            "escalated planner surface should remain modestly above max_tool_definitions cap (got {})",
            all_tiers.len()
        );
    }
}
