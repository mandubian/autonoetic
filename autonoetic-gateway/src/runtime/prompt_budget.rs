//! Prompt Budget Transparency.
//!
//! Provides observability into what consumes the context window budget
//! before each LLM call, and tool tiering for dynamic tool filtering.

use crate::llm::{Message, ToolDefinition};
use serde::Serialize;

/// Heuristic: ~4 characters per token (works across most models).
const CHARS_PER_TOKEN: f64 = 4.0;

/// Estimated overhead tokens per tool definition (name + description + schema structure).
const TOOL_OVERHEAD_TOKENS: usize = 30;

/// Tool tier for progressive disclosure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum ToolTier {
    /// Always available: content, knowledge basics, artifact basics.
    Core,
    /// Workflow-dependent: agent, workflow, evaluation.
    Workflow,
    /// Specialized: web search, promotion, advanced revision ops.
    Specialized,
}

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
fn estimate_tool_definition(tool: &ToolDefinition) -> usize {
    let name_tokens = estimate_tokens(&tool.name);
    let desc_tokens = estimate_tokens(&tool.description);
    let schema_tokens =
        estimate_tokens(&serde_json::to_string(&tool.input_schema).unwrap_or_default());
    name_tokens + desc_tokens + schema_tokens + TOOL_OVERHEAD_TOKENS
}

/// Get the tier for a tool by name prefix.
pub fn tool_tier(tool_name: &str) -> ToolTier {
    match tool_name {
        n if n.starts_with("content.") => ToolTier::Core,
        n if n.starts_with("knowledge.store") => ToolTier::Core,
        n if n.starts_with("knowledge.recall") => ToolTier::Core,
        n if n.starts_with("knowledge.search_by_tags") => ToolTier::Core,
        n if n.starts_with("knowledge.search") => ToolTier::Core,
        n if n.starts_with("artifact.build") => ToolTier::Core,
        n if n.starts_with("artifact.inspect") => ToolTier::Core,
        n if n.starts_with("sandbox.exec") => ToolTier::Core,
        n if n.starts_with("agent.spawn") => ToolTier::Workflow,
        n if n.starts_with("agent.exists") => ToolTier::Workflow,
        n if n.starts_with("agent.discover") => ToolTier::Workflow,
        n if n.starts_with("approval.") => ToolTier::Workflow,
        n if n.starts_with("workflow.") => ToolTier::Workflow,
        n if n.starts_with("eval.") => ToolTier::Workflow,
        n if n.starts_with("user.") => ToolTier::Workflow,
        n if n.starts_with("digest.") => ToolTier::Workflow,
        n if n.starts_with("web.") => ToolTier::Specialized,
        n if n.starts_with("execution.") => ToolTier::Specialized,
        n if n.starts_with("promotion.") => ToolTier::Specialized,
        n if n.starts_with("agent.revision.") => ToolTier::Specialized,
        _ => ToolTier::Specialized,
    }
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
        assert_eq!(tool_tier("content.write"), ToolTier::Core);
        assert_eq!(tool_tier("content.read"), ToolTier::Core);
        assert_eq!(tool_tier("knowledge.store"), ToolTier::Core);
        assert_eq!(tool_tier("sandbox.exec"), ToolTier::Core);
    }

    #[test]
    fn test_tool_tier_workflow() {
        assert_eq!(tool_tier("agent.spawn"), ToolTier::Workflow);
        assert_eq!(tool_tier("workflow.wait"), ToolTier::Workflow);
        assert_eq!(tool_tier("approval.status"), ToolTier::Workflow);
    }

    #[test]
    fn test_tool_tier_specialized() {
        assert_eq!(tool_tier("web.search"), ToolTier::Specialized);
        assert_eq!(tool_tier("web.fetch"), ToolTier::Specialized);
        assert_eq!(tool_tier("promotion.record"), ToolTier::Specialized);
    }

    #[test]
    fn test_filter_tools_by_tier() {
        let tools = vec![
            ToolDefinition {
                name: "content.write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web.search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "agent.spawn".to_string(),
                description: "Spawn agent".to_string(),
                input_schema: serde_json::json!({}),
            },
        ];

        let core_only = filter_tools_by_tier(tools.clone(), &[ToolTier::Core]);
        assert_eq!(core_only.len(), 1);
        assert_eq!(core_only[0].name, "content.write");

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
            name: "content.write".to_string(),
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
            name: "content.write".to_string(),
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
                name: "content.write".to_string(),
                description: "Write content".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {"name": {"type": "string"}}}),
            },
            ToolDefinition {
                name: "web.search".to_string(),
                description: "Search web".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            },
        ];

        let compressed = compress_tool_definitions(tools, 3);
        assert_eq!(compressed.len(), 2);
        assert_eq!(compressed[0].input_schema, serde_json::json!({}));
        assert_eq!(compressed[1].input_schema, serde_json::json!({}));
        assert_eq!(compressed[0].name, "content.write");
        assert_eq!(compressed[1].name, "web.search");
    }
}
