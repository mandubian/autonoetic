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
  # DEPRECATED (no-op). Tool schemas are never compressed; tool tokens are
  # saved losslessly via provider tool-array caching (prompt_cache_enabled).
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
| `demote_tools` | Drop Specialized-tier tools, keep Core + Workflow. |

Strategy names match those emitted in `GovernorAction` diagnostics and
causal events. (A `tool_schema_compression` strategy previously appeared
here but was removed: stripping tool schemas to a minimal `{"type": "object"}` placeholder corrupted tool-calling
on turn 1+ — the model needs the full schema on every turn, and prompt
caching is a billing optimization, not a "remember the tools" mechanism.
Tool tokens are now saved losslessly via provider tool-array caching; see
`prompt_cache_enabled`.)

### Section Caps

`system_prompt_max_tokens` and `tool_definitions_max_tokens` are enforced **independently** of the total budget. A section cap violation triggers the configured action even if the total is under the context window limit:

- **System prompt over cap**: Fails for all actions except `warn` (no action can reduce system prompt size at runtime)
- **Tool definitions over cap**: Fails for all actions except `warn` and `demote_tools` (which can reduce tool count)

## Wire-Format History Sanitization

Before a `CompletionRequest` is sent to the LLM, the gateway can cheaply reduce
tokens in the wire-format copy of the conversation history while keeping the
full messages in storage (checkpoints, exports, timeline events):

- **Strip reasoning content**: Remove `reasoning_content` / `reasoning_details`
  from assistant messages. The model does not need to re-read its own
  chain-of-thought on subsequent turns.
- **Truncate tool results**: Cap tool-result message content to a configured
  character budget. JSON results have their large string *values* shortened
  in-place (`content`, `stdout`, `result`…) so the JSON structure and all
  small metadata fields (`ok`, `offset`, `next_offset`, `total_bytes`,
  `truncated`, `error_type`) remain intact and parseable. This is critical
  for pagination: the agent can still read `next_offset` / `total_bytes` to
  page through large content even when the current chunk was truncated.
  Non-JSON results fall back to a whole-string `head + "[... N chars truncated ...]" + tail`
  so status/summary remains visible.
- **Deduplicate tool results**: Collapse duplicate tool-result
  messages to a short marker after the first occurrence. Re-reading artifacts,
  polling status tools, or repeated workflow snapshots often produce identical
  output across turns.

All three are controlled under `prompt_budget`:

```yaml
prompt_budget:
  strip_reasoning_from_request: false  # default; enable only if your model
                                       # does not require reasoning replay
  max_tool_result_chars: 4000          # default; set 0 to disable
  dedup_tool_results: true             # default
```

These reductions apply only to the request sent to the provider; stored history
remains complete for audit, replay, and compression strategies.

## Soft Budget (Proactive Governor)

By default, the context governor only runs when the prompt exceeds
`context_window - margin_tokens`. For large context-window models (e.g. 200K
tokens), this means the context can grow to ~196K before any summarization
happens, wasting tokens on every round.

Set `prompt_budget.soft_budget_tokens` to trigger the governor earlier:

```yaml
prompt_budget:
  soft_budget_tokens: 40000
```

When `total_tokens` exceeds `soft_budget_tokens`, the governor runs the same
reduction pipeline but targets the soft budget instead of the hard window
limit. This caps context growth before it becomes expensive. The hard limit
remains the safety backstop.

### Governor metrics in session reports (#842)

Every time the governor recovers within budget, the session report records it:

- `context_governor.fired_count` / `context_governor.tokens_saved_estimate` —
  session-level rollup in `session_report.json` (omitted when the governor
  never fired)
- `governor_fired_count` / `governor_tokens_saved_estimate` — per-agent fields
- "Governor fires" / "Governor tokens saved (est.)" rows in
  `session_overview.md`, `session_report.md`, and the HTML reports

`tokens_saved_estimate` sums `tokens_before - tokens_after` per governor run.
It measures the immediate prompt-size reduction, not the compounding savings
on subsequent turns (each later turn also avoids re-sending the removed
tokens), so real savings are typically higher.

## Tool Tiers

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

## Tool Tokens & Prompt Caching

Tool definitions are sent in full on every turn — the model needs the
complete schema (property names, types, required fields) to call tools
correctly, and there is no "the model remembers turn-0 tools" mechanism
in any provider's tools API. A previous `compress_tool_schemas_after_turn_0`
option stripped schemas to `{"type": "object"}` on turn 1+ to save tokens;
it has been **removed** because it corrupted tool-calling (hallucinated
parameters, missing required fields).

Token cost for the (large, byte-stable) tool catalog is instead recovered
**losslessly** via provider prompt caching. When `prompt_cache_enabled` is
`true` (default), cache-capable drivers attach `cache_control: {type: ephemeral}`
to both the stable system prefix and the last tool definition:

- **Anthropic** — caches the tools block + the system prefix (2 of the 4
  allowed breakpoints).
- **OpenRouter** routing a **Claude/Gemini** model — same `cache_control`
  passthrough on tools and system.
- **OpenAI** / **llama.cpp** / plain OpenAI-compatible — cache automatically
  by prefix; no markers are emitted (they would be ignored or rejected).
- **Gemini direct API** — no wire-format `cache_control` field (caching is a
  separate explicit Cached Content resource, out of scope here).

Repeated turns re-read the full tool catalog at cache rates. The
`compress_tool_schemas_after_turn_0` config field is retained only for
backward-compat and is a no-op.

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

- [Session budgets](../../reference/budgets.md) — per-session token/round/time/USD limits, enforcement flow, and the OpenRouter catalog
- [Agent Capabilities](../../AGENTS.md#capabilities-system) — capability system for tool access control
