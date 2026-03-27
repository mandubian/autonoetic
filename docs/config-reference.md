# Gateway Configuration Reference

Full reference for `config.yaml`, the gateway daemon configuration file.

Generate a default config with:

```bash
autonoetic agent init-config --output config.yaml
```

All fields have serde defaults — omitting a field uses the documented default.
Fields marked **required** must be present or the gateway will fail to start.

---

## Top-Level Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `agents_dir` | string (path) | `"./agents"` | **Required.** Directory containing agent subdirectories, each with a `SKILL.md`. Set to absolute path by `init-config`. |
| `port` | u16 | `4000` | Port for the local JSON-RPC IPC listener (Unix socket on Linux, TCP fallback). |
| `ofp_port` | u16 | `4200` | Open Fang Protocol federation port for gateway-to-gateway communication. |
| `tls` | bool | `false` | Enable TLS on the OFP port. |
| `default_lead_agent_id` | string | `"planner.default"` | Default lead agent for ambiguous ingress when no `target_agent_id` is specified. |
| `node_id` | string | `"gateway"` | Node identity for OFP federation and causal chain authorship. Overridable by `AUTONOETIC_NODE_ID` env var. |
| `node_name` | string | `"gateway"` | Human-readable node name for OFP federation. Overridable by `AUTONOETIC_NODE_NAME` env var. |
| `max_concurrent_spawns` | usize | `8` | Maximum agent runtime executions allowed concurrently across all sessions. |
| `max_pending_spawns_per_agent` | usize | `4` | Maximum pending executions admitted per target agent (includes the currently running execution). |
| `approval_timeout_secs` | u64 | `600` | Maximum seconds a workflow task can remain in `AwaitingApproval` before auto-failing. `0` disables (not recommended for production). |
| `evidence_mode` | string | `"full"` | Evidence storage mode. `"full"`: all tool/LLM results (development). `"errors"`: only failures, approval gates, non-zero exit codes (production recommended). `"off"`: no evidence files (causal chain still captures everything). |

> **Note:** `AUTONOETIC_SHARED_SECRET` is intentionally not in config.yaml — it must be set as an environment variable to avoid accidental commits of secrets.

---

## Background Scheduler

Controls the gateway-owned scheduler that periodically checks for due background agents.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `background_scheduler_enabled` | bool | `true` | Enable the background scheduler. |
| `background_tick_secs` | u64 | `5` | Interval in seconds between due-check ticks. |
| `background_min_interval_secs` | u64 | `60` | Global minimum reevaluation interval across all agents. Prevents agents from setting arbitrarily short intervals. |
| `max_background_due_per_tick` | usize | `32` | Maximum number of due background agents admitted per scheduler tick. |

---

## Sandbox

Bubblewrap isolation overrides. Environment variables always take precedence over config values.

| Field | Type | Default | Env var override | Description |
|-------|------|---------|------------------|-------------|
| `sandbox.share_net` | bool | `false` | `AUTONOETIC_BWRAP_SHARE_NET` | Share host network namespace (`--share-net`). Use when the host/kernel blocks loopback setup in isolated namespaces. |
| `sandbox.dev_mode` | string | `"legacy"` | `AUTONOETIC_BWRAP_DEV_MODE` | `/dev` mount strategy: `"legacy"` (no override), `"minimal"` (`--dev /dev`), `"host-bind"` (`--dev-bind /dev /dev`, least isolated). |

Example:

```yaml
sandbox:
  share_net: false
  dev_mode: host-bind
```

---

## Response Validation & Repair

When enabled, the gateway validates agent outputs against declared constraints in agent metadata before returning results to the caller. Repair mode adds bounded retry loops when validation fails.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `response_validation.enabled` | bool | `false` | Enable response validation. Validates agent spawn results against declared schemas/constraints. |
| `response_validation.repair_enabled` | bool | `false` | Enable bounded repair loop. When validation fails, the gateway constructs a repair prompt with the error details and retries (up to an internal limit). Requires `enabled: true`. |

Example:

```yaml
response_validation:
  enabled: true
  repair_enabled: true
```

See `docs/response-validation-gate.md` for implementation details and `docs/iteration-repair-validation-runbook.md` for the repair runbook.

---

## Agent Install Approval

Controls when `agent.install` requires human approval before proceeding.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `agent_install_approval_policy` | string | `"risk_based"` | `"always"`: every install needs approval. `"risk_based"`: only high-risk installs (broad capabilities, ShellExec, background reevaluation). `"never"`: no approval, rely on promotion gate only. |

---

## Schema Enforcement

Validates `agent.spawn` payloads against declared input schemas in agent metadata.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `schema_enforcement.mode` | string | `"deterministic"` | Enforcement mode. `"disabled"`: pass through without checks. `"deterministic"`: type coercion, defaults, required field checks. `"llm"`: (future) LLM-based transformation. |
| `schema_enforcement.audit` | bool | `true` | Log all enforcement decisions to the causal chain. |
| `schema_enforcement.agent_overrides` | map | `{}` | Per-agent mode overrides. Key = agent ID, value = mode. |

Example:

```yaml
schema_enforcement:
  mode: deterministic
  audit: true
  agent_overrides:
    my.script.agent: disabled    # skip enforcement for this agent
```

See `docs/schema-enforcement-hook.md` for details.

---

## Code Analysis

Controls how the gateway analyzes agent code during `agent.install` for capabilities and security.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `code_analysis.capability_provider` | string | `"pattern"` | Provider for capability analysis: `"pattern"`, `"python_ast"`, `"llm"`, `"composite"`, `"none"`. |
| `code_analysis.security_provider` | string | `"pattern"` | Provider for security analysis: `"pattern"`, `"python_ast"`, `"llm"`, `"composite"`, `"none"`. |
| `code_analysis.require_capabilities` | bool | `true` | Reject installs that lack declared capabilities. |
| `code_analysis.require_approval_for` | list | `["NetworkAccess", "CodeExecution"]` | Capability types that always require human approval when detected. |

### LLM-based analysis (when provider is `"llm"` or `"composite"`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `code_analysis.llm_config.provider` | string | `"openrouter"` | LLM provider for analysis. |
| `code_analysis.llm_config.model` | string | `"google/gemini-3-flash-preview"` | Model for code analysis. |
| `code_analysis.llm_config.temperature` | float | `0.1` | Temperature (lower = more deterministic). |
| `code_analysis.llm_config.timeout_secs` | u64 | `30` | Analysis timeout in seconds. |

See `docs/code-analysis.md` for details.

---

## Session Budget

Optional per-session resource limits. All fields are optional; omitting them means unlimited for that dimension.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `session_budget.profile` | string | `null` | Optional profile name for logging/ops (e.g. `"dev"`, `"production"`). |
| `session_budget.max_llm_rounds` | u64 | `null` | Maximum LLM `complete()` calls per session (each provider round-trip, including retries). |
| `session_budget.max_tool_invocations` | u64 | `null` | Maximum tool invocations per session (each call in a batch counts). |
| `session_budget.max_llm_tokens` | u64 | `null` | Maximum total LLM tokens (input + output) reported by providers per session. |
| `session_budget.max_wall_clock_secs` | u64 | `null` | Maximum wall-clock seconds from first budget touch. |
| `session_budget.max_session_price_usd` | float | `null` | Maximum estimated spend in USD (OpenRouter pricing). |
| `session_budget.extensions` | list | `[]` | Reserved for future budget extension modules. |

Example:

```yaml
session_budget:
  profile: staging
  max_llm_rounds: 120
  max_tool_invocations: 400
  max_llm_tokens: 5000000
  max_wall_clock_secs: 7200
```

See `docs/session-budget.md` and `docs/budget-management.md` for details.

---

## Retention

Controls pruning of historical data. Values are in days; `0` means retain forever.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `retention.execution_traces_days` | u32 | `30` | Days to retain `execution_traces` (full code execution results: stdout, stderr, exit_code). |
| `retention.causal_events_days` | u32 | `90` | Days to retain `causal_events` (hash-chained audit trail in SQLite). |

Example:

```yaml
retention:
  execution_traces_days: 30
  causal_events_days: 90
```

---

## Post-Session Digest

LLM summarization and Tier-2 memory extraction after agent sessions complete. Off by default.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `digest_agent.enabled` | bool | `false` | Run the digest step after eligible sessions complete. |
| `digest_agent.min_turns` | u32 | `2` | Skip digest when the session's `turn_counter` is below this value. |
| `digest_agent.llm_preset` | string | `null` | Use `llm_presets[<name>]` for provider/model/temperature. |
| `digest_agent.provider` | string | `null` | Inline provider (used when `llm_preset` is not set). |
| `digest_agent.model` | string | `null` | Inline model (used when `llm_preset` is not set). |

Example:

```yaml
digest_agent:
  enabled: true
  min_turns: 2
  llm_preset: agentic
```

---

## LLM Presets

Named LLM configurations that agents can reference by name. Used during `agent bootstrap` and `agent init --template <name>`.

```yaml
llm_presets:
  agentic:
    provider: "openrouter"
    model: "minimax/minimax-m2.7"
    temperature: 0.2
  coding:
    provider: "openrouter"
    model: "minimax/minimax-m2.7"
    temperature: 0.1
```

### LlmPreset fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `provider` | string | Yes | LLM provider: `"openai"`, `"anthropic"`, `"openrouter"`, etc. |
| `model` | string | Yes | Model identifier. |
| `temperature` | float | No | Sampling temperature. |
| `fallback_provider` | string | No | Fallback provider if primary fails. |
| `fallback_model` | string | No | Fallback model if primary fails. |
| `chat_only` | bool | No | Set `true` if the provider only supports basic chat (no tools). |
| `context_window_tokens` | u32 | No | Context window size for CLI "% of context" display when preset is applied to a SKILL. |

---

## Template → Preset Mapping

Maps role/template names to preset names. Used during `agent bootstrap` and `agent init --template <name>`.

```yaml
llm_preset_mapping:
  planner: agentic
  researcher: research
  architect: agentic
  coder: coding
  debugger: coding
  auditor: agentic
  evaluator: agentic
  specialized_builder: agentic
  default: agentic
```

The key is the template role name; the value must be a key in `llm_presets`.
When no mapping exists for a template, the agent uses its role-specific hardcoded default.

---

## Full Example

```yaml
agents_dir: "/home/user/autonoetic/agents"
port: 4000
ofp_port: 4200
tls: false
default_lead_agent_id: "planner.default"
node_id: "gateway"
node_name: "gateway"
max_concurrent_spawns: 8
max_pending_spawns_per_agent: 4
approval_timeout_secs: 600
evidence_mode: full

background_scheduler_enabled: true
background_tick_secs: 5
background_min_interval_secs: 60
max_background_due_per_tick: 32

sandbox:
  share_net: false
  dev_mode: legacy

response_validation:
  enabled: true
  repair_enabled: true

agent_install_approval_policy: risk_based

schema_enforcement:
  mode: deterministic
  audit: true

code_analysis:
  capability_provider: pattern
  security_provider: pattern
  require_capabilities: true
  require_approval_for:
    - NetworkAccess
    - CodeExecution

session_budget:
  profile: dev
  max_llm_rounds: 200
  max_tool_invocations: 500

retention:
  execution_traces_days: 30
  causal_events_days: 90

digest_agent:
  enabled: true
  min_turns: 2
  llm_preset: agentic

llm_presets:
  agentic:
    provider: "openrouter"
    model: "minimax/minimax-m2.7"
    temperature: 0.2
  coding:
    provider: "openrouter"
    model: "minimax/minimax-m2.7"
    temperature: 0.1
  research:
    provider: "openrouter"
    model: "minimax/minimax-m2.7"
    temperature: 0.3
  fallback:
    provider: "openai"
    model: "gpt-4o"
    temperature: 0.2

llm_preset_mapping:
  planner: agentic
  researcher: research
  architect: agentic
  coder: coding
  debugger: coding
  auditor: agentic
  evaluator: agentic
  specialized_builder: agentic
  default: agentic
```
