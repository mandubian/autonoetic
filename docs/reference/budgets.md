# Session budgets

Role-agnostic limits on **how much work** one **session id** may consume across
all agents sharing that session — the lead run plus every nested `agent_spawn`
run using the same session id.

This complements two other limiters:

- **`AgentSpawn.max_children`** (per agent manifest) — how many child runs an
  agent may start per session.
- **`LoopGuard`** (fixed inner loop) — consecutive LLM steps without a
  successful tool call inside a single agent run.

## Scope

- **Session id** — all counters are keyed by the same session id used for
  `agent_spawn`, chat, and nested runs. One budget pool per session per gateway
  process.
- **Process-local** — `SessionBudgetRegistry` is in-memory; restarting the
  gateway resets counters.
- **Not a billing system** — USD figures are **estimates** from OpenRouter's
  public model list, not provider invoices.

## Configuration

Set optional limits under `session_budget` in the gateway YAML (see
`autonoetic-gateway::config::load_config`). Omit a field or leave a limit unset
for **unlimited** in that dimension.

```yaml
# Example — tune for your environment
session_budget:
  profile: production
  max_llm_rounds: 200           # each provider completion() call, including retries
  max_tool_invocations: 800     # each tool call in a batch counts
  max_llm_tokens: 8_000_000     # sum of reported input + output tokens
  max_wall_clock_secs: 14_400   # wall time from first budget touch for this session
  max_session_price_usd: 2.5    # optional: estimated USD cap (see below)
  extensions: []                # reserved: future named gateway modules
```

## Semantics

| Limit | When enforced |
|--------|----------------|
| `max_llm_rounds` | Before each LLM completion; incremented after each real provider call (skipped when middleware uses `skip_llm`). |
| `max_llm_tokens` | After each completion, using provider-reported usage (often `0` if the API omits usage). |
| `max_tool_invocations` | Before executing a tool batch (`ToolUse`); reserves `len(tool_calls)`. |
| `max_wall_clock_secs` | Checked at the start of each LLM pre-check; the clock starts on first use of that session in the registry. |
| `max_session_price_usd` | After each real LLM completion, adds an **estimated** USD cost from the OpenRouter catalog (`pricing.prompt` / `pricing.completion` × token counts). Requires `llm_config.provider: openrouter` and a model id present in the catalog; if the estimate is missing, spend is not accumulated toward this cap. |

Counters live in an in-memory `SessionBudgetRegistry` shared by the gateway
process (`GatewayExecutionService`). They are **not** persisted across gateway
restarts.

## Enforcement flow

1. **Before each LLM attempt** — `check_pre_llm` (wall clock, max LLM rounds).
2. **After each real completion**, i.e. middleware did **not** set `skip_llm` —
   `record_llm_completion` (token totals, optional estimated USD, increments
   the round count).
3. **Before each tool batch** — `reserve_tool_invocations`.

Hooks live in `runtime/lifecycle.rs` (`AgentExecutor::execute_with_history`).

### Rounds where the LLM is skipped

If pre-process middleware sets `skip_llm: true`, then for that iteration there
is no provider call, no cost estimate, and no `record_llm_completion`. The
context percentage is omitted (`context_window_tokens` is forced to `None` for
that round in the tracer path).

## The OpenRouter catalog

The catalog wraps OpenRouter's public
[Models API](https://openrouter.ai/docs/guides/overview/models)
(`GET https://openrouter.ai/api/v1/models`, no API key) and caches results
(TTL ~1 hour). It supplies two things: **context window size** and **estimated
price**.

| Environment variable | Effect |
|---|---|
| `AUTONOETIC_OPENROUTER_CATALOG` | Set to `0`, `false`, `no`, or `off` to disable fetching — no context-from-catalog, no price estimates |
| `AUTONOETIC_OPENROUTER_MODELS_URL` | Override the list URL |

An `OpenRouterCatalog` instance is attached when the **gateway** builds an
`AgentExecutor` (`GatewayExecutionService`, sharing the gateway's
`reqwest::Client`), or when the **CLI** runs an agent
(`run_agent_with_runtime_with_driver`, dedicated client). If
`openrouter_catalog` is `None`, no lookups run at all.

### Maximum context (`context_length`)

**When:** once per `execute_with_history` invocation, before the main agent loop.

**Function:** `resolve_context_window_for_run` →
`OpenRouterCatalog::context_length_for_model(model_id)`, but **only if** all
four hold:

1. the manifest does **not** set `llm_config.context_window_tokens`, **and**
2. `AUTONOETIC_LLM_CONTEXT_WINDOW` is unset, **and**
3. `llm_config.provider` is `openrouter` (case-insensitive), **and**
4. the `AgentExecutor` was given `with_openrouter_catalog(Some(…))`.

Otherwise the catalog is **not** queried for context — manifest and env win.

**Why:** the resolved value drives the "% of context" UX (logs, CLI,
`llm_usage.context_window_tokens` / `input_context_pct`) for that run. It is
**not** sent to the provider as a hard limit.

**Network:** `context_length_for_model` calls `refresh_if_needed`, which may
`GET` the models list if the cache is empty or stale.

### Estimated price (`pricing` → USD)

**When:** after every real LLM completion — specifically *after* post-process
middleware runs, and only if `skip_llm` is false.

**Function:** `OpenRouterCatalog::estimate_cost_usd(model_id, input_tokens,
output_tokens)` multiplies token counts by the cached per-token
`pricing.prompt` / `pricing.completion` for that model id.

**Used for:** `LlmExchangeUsage.estimated_cost_usd` (JSON-RPC / CLI), and
`max_session_price_usd` — `record_llm_completion` adds the estimate to the
session's running USD total. If the estimate is `None`, **no USD is added** for
that completion, so the cap may never trigger.

**Network:** same cache as context; `estimate_cost_usd` also triggers
`refresh_if_needed`, usually without extra HTTP.

## LLM token usage in logs and the CLI

Each real completion records input/output tokens from the provider into session
evidence and the timeline, returns them over JSON-RPC as `llm_usage` (array of
`{ model, input_tokens, output_tokens, context_window_tokens?,
input_context_pct?, estimated_cost_usd? }`), and prints a summary on **stderr**
for `autonoetic agent run` and interactive mode.

To show the approximate **% of context window** used by the prompt
(`input_tokens` / window), one of these must supply the window:

1. `context_window_tokens` under `llm_config` in the agent `SKILL.md`, or
2. the `AUTONOETIC_LLM_CONTEXT_WINDOW` env var (used when the manifest omits
   the field), or
3. for OpenRouter agents, the catalog's `context_length` for the model id.

If none apply, totals and per-round token counts still appear and only the
percentage line is omitted. When catalog pricing is available,
`estimated_cost_usd` is included — a rough estimate from public list prices.

## Extending this

- **Config-first:** add optional fields to `SessionBudgetConfig` in
  `autonoetic-types`, wire them in `runtime/session_budget.rs`, document them
  here.
- **`extensions`:** a reserved list of names for future optional modules (for
  example org-specific rate limiters) that can grow without breaking existing
  YAML.
- **Custom code:** extend `SessionBudgetRegistry` or call it from new hook
  points. Keep policy **session-scoped**, not role-aware.

## Related code

| Component | Path |
|-----------|------|
| Budget registry | `autonoetic-gateway/src/runtime/session_budget.rs` |
| OpenRouter cache + pricing | `autonoetic-gateway/src/runtime/openrouter_catalog.rs` |
| Lifecycle: resolution + `record_llm_completion` | `autonoetic-gateway/src/runtime/lifecycle.rs` |
| Config struct | `autonoetic_types::config::SessionBudgetConfig` |
| Executor wiring | `AgentExecutor::with_session_budget` |
