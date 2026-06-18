# Prompt Budget Transparency

Autonoetic tracks and controls the token budget of each LLM request **before** it is sent to the provider. This prevents context window overflows, provides observability into what consumes the budget, and offers enforcement strategies when limits are approached or exceeded.

## Overview

Before every LLM completion, the gateway computes a **prompt budget breakdown**:

| Component | Description |
|-----------|-------------|
| **System prompt** | Foundation instructions + agent-specific instructions |
| **Tool definitions** | All available tool schemas (name + description + JSON schema) |
| **Conversation history** | All messages except system (user asks + assistant replies + tool results) |
| **Total** | Sum of all components |
| **Utilization %** | Total tokens / context window (when known) |

The breakdown is logged to the causal chain and `tracing::info!` for every turn.

## Configuration

Add a `prompt_budget` section to your gateway YAML:

```yaml
prompt_budget:
  # Hard cap for system prompt tokens (0 = unlimited)
  system_prompt_max_tokens: 8000
  # Hard cap for tool definition tokens (0 = unlimited)
  tool_definitions_max_tokens: 4000
  # Warning threshold: log warning when utilization exceeds this %
  warn_at_pct: 80
  # Safety margin subtracted from context window before enforcement
  margin_tokens: 1024
  # Whether to compress tool schemas to {"type": "object"} on turns after turn 0
  # Default: false — stripping schemas causes LLM tool-call divergence
  compress_tool_schemas_after_turn_0: false
```

### Reduction Cascade (Context Governor)

When utilization exceeds `context_window - margin_tokens`, the governor runs
its strategies in order. Each strategy returns either `Resolved` (within
budget) or `Insufficient` (try the next one). Exhausting the pipeline
classifies the turn as `context_overflow`.

| Strategy | Behavior |
|----------|----------|
| `trim_history` | Remove oldest message groups, preserving tool-call/result pairs. |
| `capsule` | Hierarchical state-capsule summarization of old turns (LLM call). |
| `tool_schema_compression` | Replace tool JSON schemas with `{"type": "object"}` placeholders. Last resort — damages tool-calling accuracy. |
| `demote_tools` | Drop Specialized-tier tools, keep Core + Workflow. |

Strategy names match those emitted in `GovernorAction` diagnostics and
causal events. Schema compression runs late in the pipeline because
stripping tool schemas causes the LLM to hallucinate parameters and
diverge from the expected tool-call contract.

### Section Caps

`system_prompt_max_tokens` and `tool_definitions_max_tokens` are enforced **independently** of the total budget. A section cap violation triggers the configured action even if the total is under the context window limit:

- **System prompt over cap**: Fails for all actions except `warn` (no action can reduce system prompt size at runtime)
- **Tool definitions over cap**: Fails for all actions except `warn` and `demote_tools` (which can reduce tool count)

## Tool Tiers

Tools are classified into three tiers for progressive disclosure and budget enforcement:

| Tier | Tools | When included |
|------|-------|---------------|
| **Core** | `content.*`, `knowledge.*`, `artifact.*`, `sandbox_exec` | Always (unless explicitly filtered) |
| **Workflow** | `agent_spawn`, `agent_discover`, `approval.*`, `workflow.*`, `federation.*`, `promotion_query`, `credential.*`, `skill_normalize`, `scheduler.*`, `eval.*`, `user.*`, `digest.*` | Default; excluded by `core_only` filtering |
| **Specialized** | `web.*`, `execution.*`, `promotion_record`, `agent.revision.*`, and uncategorized tools | Excluded by `demote_tools` and `core_and_workflow` filtering |

### Manifest-Level Tier Filtering

Agents can restrict which tool tiers they are exposed to by declaring `allowed_tool_tiers` in their SKILL.md:

```yaml
metadata:
  autonoetic:
    agent:
      id: "my-agent"
    # Only expose Core-tier tools to this agent
    allowed_tool_tiers:
      - core
```

When unset (default), the tier filter is determined by runtime state:

| State | Filter | Rationale |
|-------|--------|-----------|
| Root session, no pending approvals | All tiers | Full capability surface |
| Root session, pending approvals | Core + Workflow + approval tools | Prevents agents from launching new specialized operations while waiting for human approval |
| Child session (no explicit tiers) | Core only | Child agents get minimal tools unless parent explicitly requests more via `allowed_tool_tiers` |
| Manifest declares `allowed_tool_tiers` | Explicit tiers + approval tools | Agent-declared restriction always takes precedence |

The approval-exception ensures that `approval_status`, `approval_withdraw`, and other approval-prefixed tools are always available when approvals are pending, so the agent can check and respond to approval decisions.

## Tool Schema Compression

When `compress_tool_schemas_after_turn_0` is `true`, tool definitions on turn 1+ have their JSON schemas replaced with `{"type": "object"}`. This saves significant tokens for agents with many tools.

**Disabled by default.** Stripping tool schemas damages the LLM's ability to call tools correctly — without property names, types, and required fields, the model hallucinates parameters and produces malformed tool calls. Most LLM providers also cache identical tool arrays at reduced cost (~10%), so changing schemas between turns defeats prompt caching and is counterproductive.

The context governor still compresses schemas as a **last resort** when the context budget is exhausted (after history trimming and capsule summarization), regardless of this setting.

## Foundation Layering

The system prompt is composed from layered foundation instructions. Only relevant layers are included based on the agent's capabilities and execution mode:

| Layer | Included when |
|-------|---------------|
| `foundation_core.md` | Always |
| `foundation_workflow.md` | Agent has `AgentSpawn` capability OR is in reasoning mode |
| `foundation_artifact.md` | Agent has `WriteAccess` capability |
| `foundation_script.md` | Agent is in script execution mode |
| `foundation_digest.md` | Agent has `WriteAccess` with `digest/*` scope |

This reduces system prompt size for agents that don't need all foundation content.

## Context Window Resolution

The context window for budget calculations is resolved with this priority:

1. `llm_config.context_window_tokens` in the agent manifest
2. Environment variable `AUTONOETIC_LLM_CONTEXT_WINDOW`
3. OpenRouter catalog (if provider is `openrouter` and catalog is enabled)
4. Unknown — enforcement uses `usize::MAX` (no total budget enforcement, only section caps)

## Related Code

| Component | Path |
|-----------|------|
| Breakdown + estimation | `autonoetic-gateway/src/runtime/prompt_budget.rs` |
| Reduction pipeline | `autonoetic-gateway/src/runtime/context_governor/` (`ContextGovernor::govern`) |
| Reduction strategies | `context_governor::{schema_compress, capsule, trimming, demotion}` |
| Lifecycle integration | `autonoetic-gateway/src/runtime/lifecycle.rs` (governor call site, `compose_foundation`, `determine_tool_tier_filter`) |
| Pressure observability | `autonoetic-gateway/src/runtime/budget_tracker.rs` (`emit_context_pressure_high_if_warranted`) |
| Config structs | `autonoetic_types::config::PromptBudgetConfig` |
| Tool tier enum | `autonoetic_types::agent::ToolTier` |

## Related Docs

- [Session Budget](session-budget.md) — per-session token/round/time limits
- [Budget Management](budget-management.md) — OpenRouter catalog, pricing estimates
- [Agent Capabilities](agent-capabilities.md) — capability system for tool access control
