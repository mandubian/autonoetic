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
| ~~`default_lead_agent_id`~~ | — | — | **Removed.** `event.ingest` requires an explicit `target_agent_id`; the gateway no longer has a fallback lead. Omit this field. |
| `node_id` | string | `"gateway"` | Node identity for OFP federation and causal chain authorship. Overridable by `AUTONOETIC_NODE_ID` env var. |
| `node_name` | string | `"gateway"` | Human-readable node name for OFP federation. Overridable by `AUTONOETIC_NODE_NAME` env var. |
| `constitution.source_path` | string (path) | `"docs/constitution/versions/2026.05.05/constitution.md"` | Active constitution markdown source used for digest/profile extraction. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
| `constitution.lock_path` | string (path) | `"docs/constitution/versions/2026.05.05/gateway-constitution.lock.json"` | Active constitution lock manifest. Startup refuses to boot if lock integrity checks fail. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
| `constitution.require_signature` | bool | `true` | Require a valid constitution lock signature at startup (`fail-shut` on missing/invalid signature). |
| `constitution.trusted_signers` | map<string,string> | `{ autonoetic:constitution:v1: ... }` | Trusted signer registry (`signer_id` -> base64 Ed25519 public key, 32 bytes). Used for non-`gateway:*` signer IDs. |
| `max_concurrent_spawns` | usize | `8` | Maximum agent runtime executions allowed concurrently across all sessions. |
| `max_pending_spawns_per_agent` | usize | `4` | Maximum pending executions admitted per target agent (includes the currently running execution). |
| `max_pending_approvals_per_root` | usize | `50` | Maximum concurrent pending approvals per root session. When a new request would push the count above this cap, the request is rejected with `approval_flood`. Set to `0` to disable (not recommended). Controls the R+5 / R-7.17 approval flood cap. |
| `continuation_key` | string | `null` | HMAC-SHA256 key for signing turn continuation files. When unset, the gateway derives a deterministic key from `node_id` (development convenience only). Production deployments should set this to a high-entropy secret. Rotate by changing the value — existing continuations will fail integrity verification and be rejected. |
| `approval_timeout_secs` | u64 | `600` | Maximum seconds a workflow task can remain in `AwaitingApproval` before auto-failing. `0` disables (not recommended for production). |
| `workflow_task_heartbeat_secs` | u64 \| null | `null` | Optional heartbeat interval for `Running` workflow tasks (sync + async) to refresh `updated_at` and avoid false stuck resolution during long tails. If `null`, derives from `background_tick_secs` (clamped `1..=5`). Effective range when set: `1..=30`. |
| `max_session_turns` | u32 | `12` | Maximum turns per agent session (circuit breaker for runaway loops). When exceeded, the session suspends with `MaxTurnsReached`. |
| `evidence_mode` | string | `"full"` | Evidence storage mode. `"full"`: all tool/LLM results (development). `"errors"`: only failures, approval gates, non-zero exit codes (production recommended). `"off"`: no evidence files (causal chain still captures everything). |

> **Note:** `AUTONOETIC_SHARED_SECRET` is intentionally not in config.yaml — it must be set as an environment variable to avoid accidental commits of secrets. It authenticates both the HTTP API and local JSON-RPC ingress requests.

---

## Constitution

Controls which constitutional release the gateway enforces.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `constitution.source_path` | string (path) | `"docs/constitution/versions/2026.05.05/constitution.md"` | Canonical constitution markdown used by `constitution_read`, `gateway.info`, and federation digest/profile checks. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
| `constitution.lock_path` | string (path) | `"docs/constitution/versions/2026.05.05/gateway-constitution.lock.json"` | Lock manifest containing pinned digest/version/metadata. Startup verifies this against computed values and fails shut on mismatch. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
| `constitution.require_signature` | bool | `true` | If `true`, unsigned locks are rejected and signed locks must verify. |
| `constitution.trusted_signers` | map<string,string> | `{ autonoetic:constitution:v1: ... }` | Trust store for constitution lock signatures. Keys are signer IDs, values are base64 Ed25519 public keys (32 bytes). |

On startup (and during `autonoetic agent bootstrap`), the gateway also
materializes a local immutable snapshot under `<agents_dir>/.gateway/constitution/`:

- `CURRENT`
- `ACTIVE.json`
- `versions/<version>/constitution.md`
- `versions/<version>/gateway-constitution.lock.json`

`constitution_source` path behavior:

- release lock in repo:
  `docs/constitution/versions/<version>/gateway-constitution.lock.json`
  points to
  `docs/constitution/versions/<version>/constitution.md`
- bootstrapped runtime lock in `.gateway` points to:
  `.gateway/constitution/versions/<version>/constitution.md`

This rewrite is intentional; the gateway verifies each lock against the
configured path context.

Signature trust rules:

- `signer_id` starting with `gateway:` is verified against
  `<agents_dir>/.gateway/state_attestation.ed25519.pub` and must match
  the signer fingerprint suffix.
- other `signer_id` values are resolved through
  `constitution.trusted_signers`.

For the exact v1 signature payload and serialization contract, see
`docs/constitution-signing.md`.

Example:

```yaml
constitution:
  source_path: "docs/constitution/versions/2026.05.05/constitution.md"
  lock_path: "docs/constitution/versions/2026.05.05/gateway-constitution.lock.json"
  require_signature: true
  trusted_signers:
    autonoetic:constitution:v1: "lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk="
```

To enforce the bootstrapped runtime snapshot instead of the repo docs copy:

```yaml
constitution:
  source_path: ".gateway/constitution/versions/2026.05.05/constitution.md"
  lock_path: ".gateway/constitution/versions/2026.05.05/gateway-constitution.lock.json"
  require_signature: true
  trusted_signers:
    autonoetic:constitution:v1: "lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk="
```

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

Bubblewrap isolation settings. `config.yaml` is authoritative by default.

| Field | Type | Default | Env var override | Description |
|-------|------|---------|------------------|-------------|
| `sandbox.share_net` | bool | `false` | `AUTONOETIC_BWRAP_SHARE_NET` (gated) | Share host network namespace (`--share-net`). Use when the host/kernel blocks loopback setup in isolated namespaces. |
| `sandbox.dev_mode` | string | `"legacy"` | `AUTONOETIC_BWRAP_DEV_MODE` (gated) | `/dev` mount strategy: `"legacy"` (no override), `"minimal"` (`--dev /dev`), `"host-bind"` (`--dev-bind /dev /dev`, least isolated). |

> Env overrides above are ignored unless `AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true`.

Example:

```yaml
sandbox:
  share_net: false
  dev_mode: host-bind
```

---

## Response Validation & Repair

When enabled, the gateway validates agent outputs against declared constraints in agent metadata before returning results to the caller. Repair mode adds bounded retry loops when validation fails, but only for agents that opt in via `response_contract.repair.auto`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `response_validation.enabled` | bool | `false` | Enable response validation. Validates agent spawn results against declared schemas/constraints. |
| `response_validation.repair_enabled` | bool | `false` | Enable gateway-side auto-repair subsystem. Actual repair still requires per-agent opt-in via `response_contract.repair.auto: true`. Requires `enabled: true`. |
| `response_validation.max_repair_attempts_ceiling` | u32 | `2` | System hard ceiling for auto-repair attempts. Effective attempts are `min(response_contract.repair.max_attempts, max_repair_attempts_ceiling)`. |

Example:

```yaml
response_validation:
  enabled: true
  repair_enabled: true
  max_repair_attempts_ceiling: 2
```

See `docs/response-validation-gate.md` for implementation details and `docs/iteration-repair-validation-runbook.md` for the repair runbook.

---

## Revision Promotion Approval

Controls when `agent_revision_promote` requires human approval before proceeding. The gateway gates high-risk promotions (bundles with broad capabilities or detected remote access) the same way it previously gated `agent.install`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `revision_promote_approval_policy` | string | `"risk_based"` | `"always"`: every promotion needs approval. `"risk_based"`: only high-risk promotions (NetworkAccess, CodeExecution, background reevaluation). `"never"`: no approval gate (not recommended for production). |

> **Note:** The old `agent_install_approval_policy` field is **removed**. Update existing `config.yaml` files to use `revision_promote_approval_policy`.

---

## Schema Enforcement

Validates `agent_spawn` payloads against declared input schemas in agent metadata.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `schema_enforcement.mode` | string | `"deterministic"` | Enforcement mode. `"disabled"`: pass through without checks. `"deterministic"`: type coercion, defaults, required field checks. |
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

Controls how the gateway analyzes agent code during `agent_revision_create` for capabilities and security.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `code_analysis.capability_provider` | string | `"pattern"` | Provider for capability analysis: `"pattern"`, `"python_ast"`, `"llm"`, `"composite"`, `"none"`. |
| `code_analysis.security_provider` | string | `"pattern"` | Provider for security analysis: `"pattern"`, `"python_ast"`, `"llm"`, `"composite"`, `"none"`. |
| `code_analysis.require_capabilities` | bool | `true` | Reject revision creation if code requires undeclared capabilities. |
| `code_analysis.require_approval_for` | list | `["NetworkAccess", "CodeExecution"]` | Capability types that always require human approval when detected during revision creation. |

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

## Loop Guard

Controls the per-session runaway loop detection. Independent of `max_session_turns` (which is a hard circuit breaker at the gateway level).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `loop_guard.max_loops_without_progress` | u32 | `5` | Maximum turns without meaningful progress before suspension. Reset by any tool call returning `ok: true` with a new (tool, arguments) fingerprint. |
| `loop_guard.max_tool_failures` | u32 | `5` | Maximum total failures per tool name before suspension. NOT reset by `register_progress()`. Catches alternating-failure patterns where the same tool keeps failing regardless of arguments. |
| `loop_guard.max_consecutive_same_progress` | u32 | `1` | Number of consecutive identical (tool, arguments) calls allowed before repeats stop counting as progress. Default `1` means the first call counts as progress, but the second identical call does not. |

Example:

```yaml
loop_guard:
  max_loops_without_progress: 5
  max_tool_failures: 5
  max_consecutive_same_progress: 1
```

The loop guard trips when EITHER condition is met:
1. `current_loops >= max_loops_without_progress` (no meaningful progress)
2. Any single tool's failure count reaches `max_tool_failures`

"Meaningful progress" requires a tool call with a fingerprint different from the previous `max_consecutive_same_progress` calls. This prevents agents from spinning on the same successful-but-useless tool call indefinitely.

Per-agent manifests may declare stricter loop limits under
`metadata.autonoetic.loop_guard`. Gateway treats `config.loop_guard` as the
ceiling and applies `min(declared, configured)` per field.

---

## Max Session Turns

Circuit breaker for runaway agent sessions.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_session_turns` | u32 | `12` | Maximum turns per session before forced suspension (`MaxTurnsReached`). |

---

## Prompt Budget

Controls context window transparency and enforcement for prompt construction.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `prompt_budget.system_prompt_max_tokens` | usize | `0` | Maximum tokens for system prompt (foundation + agent instructions). `0` = unlimited. |
| `prompt_budget.tool_definitions_max_tokens` | usize | `0` | Maximum tokens for all tool definitions combined. `0` = unlimited. |
| `prompt_budget.warn_at_pct` | float | `80.0` | Warn when total prompt utilization exceeds this percentage of context window. |
| `prompt_budget.margin_tokens` | usize | `4096` | Reserve this many tokens at the end of the context window for LLM output. |
| `prompt_budget.on_exceeded` | string | `"warn"` | Action when budget exceeded: `"warn"`, `"trim_history"`, `"demote_tools"`, `"fail"`. |
| `prompt_budget.compress_tool_schemas_after_turn_0` | bool | `false` | Strip tool JSON schemas to `{}` after the first turn to save tokens. |

Example:

```yaml
prompt_budget:
  system_prompt_max_tokens: 0
  tool_definitions_max_tokens: 0
  warn_at_pct: 80.0
  margin_tokens: 4096
  on_exceeded: warn
  compress_tool_schemas_after_turn_0: false
```

---

## LLM Presets

Unified registry for all LLM configurations. Each preset is either **fixed** (concrete provider/model) or **routing** (dynamic selection from fixed presets at call time).

### Fixed Preset Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `provider` | string | required (if no `routing`) | LLM provider: `"openai"`, `"anthropic"`, `"openrouter"`, etc. |
| `model` | string | required (if no `routing`) | Model identifier. |
| `temperature` | float | `null` | Sampling temperature. |
| `fallback_provider` | string | `null` | Fallback provider if primary fails. |
| `fallback_model` | string | `null` | Fallback model if primary fails. |
| `chat_only` | bool | `null` | Set `true` if the provider only supports basic chat (no tools). |
| `context_window_tokens` | u32 | `null` | Context window size for CLI "% of context" display. |
| `base_url` | string | `null` | Optional base URL for OpenAI-compatible providers (e.g., LM Studio, Ollama). |
| `api_key_env` | string | `null` | Environment variable name for the API key. Overrides the provider's default (e.g., set to `"MY_API_KEY"` for a custom OpenAI-compatible provider instead of `"OPENAI_API_KEY"`). |
| `thinking` | object | `null` | Extended thinking configuration (see `docs/AGENTS.md#extended-thinking`). When set here, all agents using this preset inherit the thinking config unless they override it in SKILL.md. |
| `tier` | string | `null` | Capability tier: `"economy"`, `"standard"`, `"premium"`. Used when referenced by routing presets. |
| `cost.input_per_million` | float | `null` | Cost per million input tokens (USD). |
| `cost.output_per_million` | float | `null` | Cost per million output tokens (USD). |
| `latency.ttft_ms` | u64 | `null` | Expected time-to-first-token (ms). |
| `latency.tokens_per_second` | u64 | `null` | Expected output throughput. |

### Routing Preset Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `routing.strategy` | string | required | `"disabled"`, `"deterministic"`, `"classifier"`, `"hybrid"`. |
| `routing.models` | list | required | Fixed preset names to route between (e.g., `[opus, sonnet, haiku]`). |
| `routing.classifier_preset` | string | `null` | Fixed preset name for the classifier model (classifier/hybrid strategies). |

#### routing.deterministic sub-object

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_tier` | string | `"premium"` | Maximum allowed tier: `"economy"`, `"standard"`, `"premium"`. |
| `max_cost_usd` | float | `null` | Max cost per session before downgrading to economy. |
| `budget_downgrade_threshold` | float | `0.8` | Budget pressure (0.0–1.0) at which to downgrade to economy. |
| `enable_fallback_chain` | bool | `true` | Retry with fallback model on failure. |

#### routing.classifier sub-object

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `timeout_secs` | u64 | `2` | Classifier call timeout. |
| `skip_threshold` | float | `0.95` | Skip classifier when budget pressure exceeds this (0.0–1.0). |

#### routing.hybrid sub-object

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `ambiguity_threshold` | float | `0.5` | Use classifier when deterministic confidence is below this (0.0–1.0). |
| `classifier` | object | (see above) | Classifier settings for hybrid fallback. |

### Validation Rules

1. A preset must have either `provider`+`model` (fixed) or `routing` (routing preset), not both, not neither.
2. `routing.models` must reference existing fixed presets only.
3. `routing.classifier_preset` must reference an existing fixed preset.
4. `llm_preset_mapping` values must reference existing presets (fixed or routing).

Example:

```yaml
llm_presets:
  # Fixed presets
  haiku:
    provider: anthropic
    model: claude-haiku-3-20250307
    tier: economy
    cost:
      input_per_million: 0.25
      output_per_million: 1.25

  sonnet:
    provider: anthropic
    model: claude-sonnet-4-20250514
    tier: standard
    cost:
      input_per_million: 3.0
      output_per_million: 15.0

  opus:
    provider: anthropic
    model: claude-opus-4-20250514
    tier: premium
    cost:
      input_per_million: 15.0
      output_per_million: 75.0

  # Routing presets
  smart:
    routing:
      strategy: hybrid
      models: [opus, sonnet, haiku]
      classifier_preset: haiku
      deterministic:
        max_tier: premium
        budget_downgrade_threshold: 0.8
        enable_fallback_chain: true
      classifier:
        timeout_secs: 2
        skip_threshold: 0.95
      hybrid:
        ambiguity_threshold: 0.5

  budget:
    routing:
      strategy: deterministic
      models: [sonnet, haiku]
      deterministic:
        max_tier: standard
        budget_downgrade_threshold: 0.6
        enable_fallback_chain: true
```

---

## LLM Routing

Cross-cutting concerns only. Model definitions and routing strategies live in `llm_presets`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `llm_routing.agent_overrides` | map | `{}` | Per-agent model/tier overrides. Key = agent ID. |
| `llm_routing.approval_gates` | object | (see below) | Approval gates for routing decisions. |

### ModelOverride

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `model` | string | `null` | Force a specific model name (must match a fixed preset's model). No provider prefix. |
| `min_tier` | string | `null` | Minimum capability tier: `"economy"`, `"standard"`, `"premium"`. |

### Approval Gates

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `approval_gates.premium_model_first_use` | bool | `false` | Require approval before first premium model use. |
| `approval_gates.budget_threshold_crossed` | float | `null` | Require approval when budget exceeds this (0.0–1.0). |

Example:

```yaml
llm_routing:
  agent_overrides:
    planner.default:
      min_tier: standard
    coder.default:
      model: claude-sonnet-4-20250514
  approval_gates:
    premium_model_first_use: false
    budget_threshold_crossed: 0.75
```

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

## Hooks

Reactive bindings from gateway events to actions. When an event fires (e.g., session closes, approval resolves), the matching hooks are dispatched.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `hooks[].on` | string | required | Event name: `session.closed`, `session.suspended`, `approval.resolved`, `approval.requested`, `workflow.join.satisfied`, `artifact.created`, `agent.promoted`, `emergency_stop`, `policy.decision` |
| `hooks[].action` | string | required | Action: `publish_report`, `deliver_signal`, `agent.spawn`, `http.callback` |
| `hooks[].async` | bool | `false` | If true, the hook runs in a background task without blocking the event |
| `hooks[].params` | object | `{}` | Action-specific parameters |
| `hooks[].callback_allowlist` | list | `[]` | Required for `http.callback`. Allowlist entries use grant-target shapes such as `{ kind: "url_prefix", value: "https://hooks.example.com/autonoetic/" }` |

Example:

```yaml
hooks:
  - on: "session.closed"
    action: "publish_report"
    async: true

  - on: "approval.resolved"
    action: "deliver_signal"
    async: true

  - on: "workflow.join.satisfied"
    action: "deliver_signal"
    async: true

  - on: "session.closed"
    action: "http.callback"
    async: true
    callback_allowlist:
      - kind: "url_prefix"
        value: "https://webhook.example.com/autonoetic/"
    params:
      url: "https://webhook.example.com/autonoetic/session-closed"
      secret_env: "AUTONOETIC_HOOK_SECRET"
```

`policy.decision` fires after selected `causal_events` inserts; see **Constitutional observability (`policy.decision`)** under [Hook System](ARCHITECTURE.md#hook-system) in `ARCHITECTURE.md`. Example: spawn an observer agent on denials and other policy-tagged outcomes:

```yaml
hooks:
  - on: "policy.decision"
    action: "agent.spawn"
    async: true
    params:
      agent_id: "constitutional-observer.default"
      message_template: |
        {{event}} root={{root_session_id}} session={{session_id}} agent={{agent_id}}
        status={{status}} decision={{decision}} rules={{rule_ids}} event_id={{event_id}}
        category={{category}} action={{action}} target={{target}} reason={{reason}}
    allowed_agents:
      - constitutional-observer.default
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

## Template → Preset Mapping

Maps role/template names to preset names. Used during `agent bootstrap` and `agent init --template <name>`.
Values can reference either fixed presets or routing presets.

```yaml
llm_preset_mapping:
  planner: smart             # routing preset
  coder: smart               # routing preset
  executor: smart            # routing preset
  researcher: sonnet         # fixed preset
  debugger: haiku            # fixed preset
  evaluator: budget          # routing preset
  architect: opus            # fixed preset
  specialized_builder: agentic
  default: sonnet
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
node_id: "gateway"
node_name: "gateway"
constitution:
  source_path: "docs/constitution/versions/2026.05.05/constitution.md"
  lock_path: "docs/constitution/versions/2026.05.05/gateway-constitution.lock.json"
  require_signature: true
  trusted_signers:
    autonoetic:constitution:v1: "lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk="
max_concurrent_spawns: 8
max_pending_spawns_per_agent: 4
approval_timeout_secs: 600
max_pending_approvals_per_root: 50
# continuation_key: "set-me-in-production-from-a-secret-source"
workflow_task_heartbeat_secs: 2
evidence_mode: full
max_session_turns: 12

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
  max_repair_attempts_ceiling: 2

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

prompt_budget:
  warn_at_pct: 80.0
  margin_tokens: 4096
  on_exceeded: warn

retention:
  execution_traces_days: 30
  causal_events_days: 90

digest_agent:
  enabled: true
  min_turns: 2
  llm_preset: agentic

llm_routing:
  agent_overrides:
    planner.default:
      min_tier: standard
  approval_gates:
    premium_model_first_use: false
    budget_threshold_crossed: 0.75

llm_presets:
  haiku:
    provider: anthropic
    model: claude-haiku-3-20250307
    tier: economy
    cost:
      input_per_million: 0.25
      output_per_million: 1.25

  sonnet:
    provider: anthropic
    model: claude-sonnet-4-20250514
    tier: standard
    cost:
      input_per_million: 3.0
      output_per_million: 15.0

  opus:
    provider: anthropic
    model: claude-opus-4-20250514
    tier: premium
    cost:
      input_per_million: 15.0
      output_per_million: 75.0

  smart:
    routing:
      strategy: hybrid
      models: [opus, sonnet, haiku]
      classifier_preset: haiku
      deterministic:
        max_tier: premium
        budget_downgrade_threshold: 0.8
        enable_fallback_chain: true
      classifier:
        timeout_secs: 2
        skip_threshold: 0.95
      hybrid:
        ambiguity_threshold: 0.5

  budget:
    routing:
      strategy: deterministic
      models: [sonnet, haiku]
      deterministic:
        max_tier: standard
        budget_downgrade_threshold: 0.6
        enable_fallback_chain: true

  agentic:
    provider: openrouter
    model: minimax/minimax-m2.7
    temperature: 0.2

  coding:
    provider: openrouter
    model: minimax/minimax-m2.7
    temperature: 0.1

  research:
    provider: openrouter
    model: minimax/minimax-m2.7
    temperature: 0.3

  fallback:
    provider: openai
    model: gpt-4o
    temperature: 0.2

llm_preset_mapping:
  planner: smart
  coder: smart
  researcher: sonnet
  debugger: haiku
  evaluator: budget
  architect: opus
  specialized_builder: agentic
  default: sonnet
```
