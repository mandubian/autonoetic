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
| `http_port` | u16 | `4100` | HTTP ingress bind (`0.0.0.0:http_port`) for `/api/*` routes (`event.ingest`, SSE, content API). Set `0` to disable HTTP while keeping localhost JSON-RPC on `port`. |
| `ofp_port` | u16 | `4200` | Open Fang Protocol federation port for gateway-to-gateway communication. |
| `tls` | bool | `false` | Enable TLS on the OFP port. |
| ~~`default_lead_agent_id`~~ | — | — | **Removed.** `event.ingest` requires an explicit `target_agent_id`; the gateway no longer has a fallback lead. Omit this field. |
| `node_id` | string | `"gateway"` | Node identity for OFP federation and causal chain authorship. Overridable by `AUTONOETIC_NODE_ID` env var. |
| `node_name` | string | `"gateway"` | Human-readable node name for OFP federation. Overridable by `AUTONOETIC_NODE_NAME` env var. |
| `constitution.source_path` | string (path) | `"docs/constitution/versions/2026.05.19/constitution.md"` | Active constitution markdown source used for digest/profile extraction. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
| `constitution.lock_path` | string (path) | `"docs/constitution/versions/2026.05.19/gateway-constitution.lock.json"` | Active constitution lock manifest. Startup refuses to boot if lock integrity checks fail. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
| `constitution.require_signature` | bool | `true` | Require a valid constitution lock signature at startup (`fail-shut` on missing/invalid signature). |
| `constitution.trusted_signers` | map<string,string> | `{ autonoetic:constitution:v1: ... }` | Trusted signer registry (`signer_id` -> base64 Ed25519 public key, 32 bytes). Used for non-`gateway:*` signer IDs. |
| `max_concurrent_spawns` | usize | `8` | Maximum agent runtime executions allowed concurrently across all sessions. |
| `max_pending_spawns_per_agent` | usize | `4` | Maximum pending executions admitted per target agent (includes the currently running execution). |
| `max_spawn_depth` | u32 | `8` | System-wide ceiling for spawn-chain depth (R+3 / R-7.15). Per-agent `AgentSpawn.max_spawn_depth` may be lower; the tighter bound wins. |
| `max_pending_approvals_per_root` | usize | `50` | Maximum concurrent pending approvals per root session. When a new request would push the count above this cap, the request is rejected with `approval_flood`. Set to `0` to disable (not recommended). Controls the R+5 / R-7.17 approval flood cap. |
| `continuation_key` | string | `null` | HMAC-SHA256 key for signing turn continuation files. When unset, the gateway derives a deterministic key from `node_id` (development convenience only). Production deployments should set this to a high-entropy secret. Rotate by changing the value — existing continuations will fail integrity verification and be rejected. |
| `approval_timeout_secs` | u64 | `600` | Maximum seconds a workflow task can remain in `AwaitingApproval` before auto-failing. `0` disables (not recommended for production). |
| `workflow_task_heartbeat_secs` | u64 \| null | `null` | Optional heartbeat interval for `Running` workflow tasks (sync + async) to refresh `updated_at` and avoid false stuck resolution during long tails. If `null`, derives from `background_tick_secs` (clamped `1..=5`). Effective range when set: `1..=30`. |
| `stuck_task_timeout_secs` | u64 \| null | `600` | Max seconds a `Running` workflow task can go without progress before the sweeper force-completes it. The task is **always resolved as `Succeeded`** (not `Failed`), using whatever exit evidence the sweeper can find (manifest exit, digest tail, implicit artifacts); when no evidence is found the task is still resolved as `Succeeded` to keep the parent workflow unblocked. `null` uses the default (600). Set to `0` to disable. |
| `approval_dwell_multiplier` | f64 | `1.0` | Multiplier applied to approval dwell times (R++4). Values above `1.0` slow down approval resolution. Set to `0` to disable dwell enforcement (tests). |
| `signal_delivery_timeout_secs` | u64 | `60` | Timeout in seconds for signal delivery responses (approval resolution, workflow join). The signal sender waits this long for the planner to finish processing the triggered `event.ingest` turn. |
| `max_session_turns` | u32 | `12` | Maximum turns per agent session (circuit breaker for runaway loops). When exceeded, the session suspends with `MaxTurnsReached`. |
| `evidence_mode` | string | `"full"` | Evidence storage mode. `"full"`: all tool/LLM results (development). `"errors"`: only failures, approval gates, non-zero exit codes (production recommended). `"off"`: no evidence files (causal chain still captures everything). |
| `capability_delta_gate_mode` | string | `"strict"` | Capability delta gating during `agent.revision.promote`: `"strict"` (any broadening requires approval), `"evolving"` (broadening inside wildcard envelopes auto-allowed), `"bootstrap"` (gating disabled, dev only). |
| `interaction_answer_orchestration` | bool | `true` | When `true`, JSON-RPC `interaction.answer` / `interaction.resolve_and_answer` persist answers and orchestrate workflow task or session resume. When `false`, the method fails fast (legacy detection). |
| `allow_runtime_lock_drift` | bool | `false` | Allow sessions to start when `runtime.lock` gateway section disagrees with the running binary (R+7 / R-8.12). Drift is still logged as a causal event. |
| `trust_unsigned_bundles` | bool | `false` | Allow revision creation without a gateway signature when the identity key is unavailable (dev escape hatch). See `docs/revision-signing.md`. |
| `profile` | string | `"standard"` | Complexity profile: `starter` (simplified UX, auto-approve safe tools), `standard` (current behavior), or `expert` (full constitutional visibility). See [Profiles](#profiles). |
| `persona_path` | string (path) \| null | `null` | Path to a Markdown file injected into every agent's system prompt. Enables cross-agent user context and communication preferences. Relative paths resolve from the config directory. When `null`, the gateway looks for `persona.md` next to the config file (used only if it exists). |

> **Note:** `AUTONOETIC_SHARED_SECRET` is intentionally not in config.yaml — it must be set as an environment variable to avoid accidental commits of secrets. It authenticates both the HTTP API and local JSON-RPC ingress requests.

---

## Constitution

Controls which constitutional release the gateway enforces.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `constitution.source_path` | string (path) | `"docs/constitution/versions/2026.05.19/constitution.md"` | Canonical constitution markdown used by `constitution_read`, `gateway.info`, and federation digest/profile checks. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
| `constitution.lock_path` | string (path) | `"docs/constitution/versions/2026.05.19/gateway-constitution.lock.json"` | Lock manifest containing pinned digest/version/metadata. Startup verifies this against computed values and fails shut on mismatch. Relative paths resolve in this order: `agents_dir/<path>`, `agents_dir` parent, current working directory, workspace root fallback. |
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
  source_path: "docs/constitution/versions/2026.05.19/constitution.md"
  lock_path: "docs/constitution/versions/2026.05.19/gateway-constitution.lock.json"
  require_signature: true
  trusted_signers:
    autonoetic:constitution:v1: "lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk="
```

To enforce the bootstrapped runtime snapshot instead of the repo docs copy:

```yaml
constitution:
  source_path: ".gateway/constitution/versions/2026.05.19/constitution.md"
  lock_path: ".gateway/constitution/versions/2026.05.19/gateway-constitution.lock.json"
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

## Fast Scheduler Sidecar

Parallel low-latency scheduling loop for interval-style jobs (`every N seconds`). Runs beside the canonical background scheduler, sharing the same DB `claim_and_advance_due_job` call so the two loops cannot double-dispatch.

Only interval-mode schedules are eligible; cron-style schedules remain on the canonical 5-second loop. Sub-10s intervals still require script-mode targets (defense-in-depth re-check at dispatch).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `fast_scheduler.enabled` | bool | `false` | Enable the fast scheduler sidecar. |
| `fast_scheduler.tick_millis` | u64 | `200` | How often the fast loop wakes up (milliseconds). |
| `fast_scheduler.max_due_per_tick` | usize | `64` | Maximum candidate jobs admitted per tick. |

Example:

```yaml
fast_scheduler:
  enabled: true
  tick_millis: 200
  max_due_per_tick: 64
```

---

## Promotion Safety Governor

Three soft gates enforced at `agent_revision_promote` time, guarding against runaway promotion velocity, flapping (re-promoting a recently-active revision), and eval regression (finding counts strictly increasing across consecutive promotions).

All three gates are bypassable via `force: true` + `force_reason` (operator override, capped at 512 characters). Bypass emits a `governor.override` causal event. Disabled by default.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `promotion_governor.enabled` | bool | `false` | Enable governor checks. |
| `promotion_governor.velocity_window_hours` | u64 | `24` | Sliding window for the velocity check (hours). |
| `promotion_governor.max_promotions_per_window` | usize | `3` | Maximum `Promote` entries per alias inside the velocity window. |
| `promotion_governor.flapping_lookback` | usize | `4` | How many most-recent promotions to scan when checking for candidate-exclusion (re-promoting a revision already seen recently). |
| `promotion_governor.eval_regression_streak` | usize | `3` | How many adjacent finding-count comparisons must be strictly increasing for the eval-regression halt to fire. |
| `promotion_governor.eval_regression_lookback` | usize | `6` | Maximum number of recent promotions to scan for the eval-regression streak. |

Example:

```yaml
promotion_governor:
  enabled: true
  velocity_window_hours: 24
  max_promotions_per_window: 3
  flapping_lookback: 4
  eval_regression_streak: 3
  eval_regression_lookback: 6
```

---

## Curator Decision Journal

When `response_validation` is enabled, the gateway automatically parses the `decision_journal` array from agent output and persists one `curator.decision` causal event per entry. This is always active when response validation is on — no separate configuration key is needed.

Each entry records:
- **target**: the memory or resource the decision applies to
- **action**: what was done (e.g. `keep`, `drop`, `merge`)
- **reason_code**: stable machine-readable code (e.g. `high_confidence_pattern`)
- **reason_detail**: optional human-readable explanation
- **metric_values**: optional structured metrics
- **confidence**: optional 0.0–1.0 confidence score

The event's `target` column carries the entry's `target` field, enabling direct queries like "why was memory X dropped?". A summary event (`decision_journal_recorded`) is also emitted per agent run with the total entry count.

The category parameter is configurable per agent type (defaults to `curator`). See `docs/security-sentinel.md` for how the sentinel's future LLM-judgment layer will audit these entries.

---

## Response Validation & Repair

When enabled, the gateway validates agent outputs against declared constraints in agent metadata before returning results to the caller. Repair mode adds bounded retry loops when validation fails, but only for agents that opt in via `io.output_policy.repair.auto`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `response_validation.enabled` | bool | `false` | Enable response validation. Validates agent spawn results against declared schemas/constraints. |
| `response_validation.repair_enabled` | bool | `false` | Enable gateway-side auto-repair subsystem. Actual repair still requires per-agent opt-in via `io.output_policy.repair.auto: true`. Requires `enabled: true`. |
| `response_validation.max_repair_attempts_ceiling` | u32 | `2` | System hard ceiling for auto-repair attempts. Effective attempts are `min(io.output_policy.repair.max_attempts, max_repair_attempts_ceiling)`. |

Example:

```yaml
response_validation:
  enabled: true
  repair_enabled: true
  max_repair_attempts_ceiling: 2
```

See `docs/response-validation-gate.md` for implementation details and `docs/iteration-repair-validation-runbook.md` for the repair runbook.

---

## Protected Agents

Controls the protected-agent promotion gate (issue #21). Agents listed here
require a passed eval run (`required_eval_run_id`) before programmatic
promotion via `agent_revision_promote` is allowed. This closes the
recursive-trust problem: a regressed agent-factory cannot silently replace
itself without independent verification.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `protected_agents.enabled` | bool | `true` | Enable the protected-agent gate. Set to `false` to disable in development. |
| `protected_agents.agents` | list\<string\> | `[]` | Agent IDs that require eval evidence for promotion. |

The gate fires after the standard capability-delta and artifact promotion
gates but before the sentinel gate. When a protected agent is promoted:

1. `required_eval_run_id` must be provided and must reference a **passed**
   eval run targeting the exact revision being promoted.
2. The standard capability-delta gate (R++2) still fires for any broadening.
3. The sentinel pre-promotion gate still fires (if enabled).

When the gate blocks promotion, the error response includes:
- `error: "protected_agent_requires_eval_run"` — identifies the gate
- `protected_agent` — the agent ID
- `repair_hint` — how to satisfy the gate

Example:

```yaml
protected_agents:
  enabled: true
  agents:
    - agent-factory.default
    - specialized_builder.default
    - evolution-orchestrator.default
```

For manual recovery of a protected agent (e.g. when the eval suite is
unavailable), use the CLI escape hatch:

```bash
# Rollback to previous revision (bypasses eval gate)
autonoetic agent revision rollback agent-factory.default

# Or: seed a specific revision directly (bypasses all gates)
autonoetic agent seed agent-factory.default <revision-id> --reason "manual recovery"
```

See `docs/protected-agents.md` for the full manual recovery procedure.

---

## Capability Delta Gate

Controls how capability broadening during `agent.revision.promote` is gated. High-risk promotion evidence (evaluator/auditor, federation jury) is separate — see `docs/AGENTS.md` and `docs/protected-agents.md`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `capability_delta_gate_mode` | string | `"strict"` | `"strict"`: any capability broadening requires explicit approval. `"evolving"`: broadening inside an existing wildcard envelope is auto-allowed. `"bootstrap"`: disable capability-delta gating (development only). |

> **Removed:** `revision_promote_approval_policy` / `agent_install_approval_policy` are no longer config fields. Promotion approval is driven by capability declarations, delta gating, protected-agent eval gates, and the sentinel promotion gate.

---

## Post-Promotion Review

Controls the background review of promoted agents (Phase 4 Tier 1). Reviews operational drift daily: tool failure rates, authorization denials, suspension counts, and new sentinel findings.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `post_promotion_review.enabled` | bool | `true` | Enable daily post-promotion review |
| `post_promotion_review.interval_secs` | u64 | `86400` | Review interval in seconds (default: 24 hours) |
| `post_promotion_review.tool_failure_rate_warning` | f64 | `1.5` | Multiplier threshold for warning (current / previous) |
| `post_promotion_review.tool_failure_rate_critical` | f64 | `3.0` | Multiplier threshold for critical escalation |
| `post_promotion_review.sentinel_findings_warning` | u64 | `0` | Sentinel findings count for warning |
| `post_promotion_review.sentinel_findings_critical` | u64 | `2` | Sentinel findings count for critical escalation |

Example:

```yaml
post_promotion_review:
  enabled: true
  interval_secs: 86400
  tool_failure_rate_warning: 1.5
  tool_failure_rate_critical: 3.0
  sentinel_findings_warning: 0
  sentinel_findings_critical: 2
```

> Critical findings trigger an `EscalationMessage` visible in `autonoetic gateway escalations list`.

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
| `loop_guard.max_child_failures` | u32 | `3` | Maximum total child-agent spawn failures before suspension. Prevents agents from repeatedly spawning failing children. |

Example:

```yaml
loop_guard:
  max_loops_without_progress: 5
  max_tool_failures: 5
  max_consecutive_same_progress: 1
  max_child_failures: 3
```

The loop guard trips when ANY of these conditions is met:
1. `current_loops >= max_loops_without_progress` (no meaningful progress)
2. Any single tool's failure count reaches `max_tool_failures`
3. Child-agent spawn failure count reaches `max_child_failures`

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
| `prompt_budget.compress_tool_schemas_after_turn_0` | bool | `false` | Strip tool JSON schemas to `{}` after the first turn to save tokens. |

When utilization exceeds the budget, the context governor cascades reduction
strategies (tool-schema compression → hierarchical capsule summarization →
history trimming → tool demotion). See [docs/prompt-budget.md](prompt-budget.md).

Example:

```yaml
prompt_budget:
  system_prompt_max_tokens: 0
  tool_definitions_max_tokens: 0
  warn_at_pct: 80.0
  margin_tokens: 4096
  compress_tool_schemas_after_turn_0: false
```

---

## Tool Tier Registry

Tool-tier assignments used by `ToolTierFilter` are declarative and loaded at gateway
startup from `config/tools.yaml`.

- Default path: `config/tools.yaml`
- Override path: env var `AUTONOETIC_TOOL_TIER_REGISTRY_PATH`
- Match rule: first `rules[].prefix` that matches `tool_name.starts_with(prefix)` wins
- Fallback: `default_tier`

Schema:

```yaml
version: 1
default_tier: specialized
rules:
  - prefix: content_
    tier: core
  - prefix: approval_
    tier: workflow
  - prefix: web_
    tier: specialized
```

`tier` values: `core`, `workflow`, `specialized`.

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

### Cross-cutting role keys

`llm_preset_mapping` keys are usually agent template names (planner, coder,
etc.), but the following cross-cutting role keys are also honored:

| Key | Effect |
|-----|--------|
| `context_compression` | Used as fallback for `context_compression.llm_preset` when that field is not set explicitly. The mapped preset must be a fixed preset (not a routing preset) — `validate_llm_presets` rejects routing presets here because the consumer needs a concrete provider/model. The fallback only fires when no compression LLM is configured at all (explicit `llm_preset`, explicit `provider`+`model`, and agent-level overrides take precedence). |

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

## Security Sentinel

System-tier read-only auditor over causal events, promotion history, approvals, layer mounts, and SKILL.md bodies. Produces append-only `SecurityFinding` records. See [`docs/security-sentinel.md`](security-sentinel.md) for the full design and threat model.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `sentinel.enabled` | bool | `true` | Master switch. When `false`, sweeps and the promotion gate are skipped. |
| `sentinel.full_sweep_schedule` | string (cron) | `"0 3 * * *"` | Cron expression for the daily full (non-incremental) dual sweep. Each check still applies its own `scan_limit` (default 10 000 events) and per-check window (e.g. `window_days = 30` for capability accretion / approval denial); "full" means no `since_rfc3339` cutoff is applied, not that every historical row is examined. UTC. |
| `sentinel.incremental_sweep_schedule` | string (cron) | `"0 */6 * * *"` | Cron expression for the rolling 25 h incremental dual sweep (sets `since_rfc3339 = now-25h`). UTC. |
| `sentinel.promotion_gate_enabled` | bool | `true` | When `true`, `agent_revision_promote` runs an in-memory Phase-1 sentinel sweep first via `scan_phase1_critical`, **scoped to the agent being promoted**: every Phase-1 check filters its query by `agent_id = <agent_being_promoted>`, so a critical finding attributed to a different agent does not block this promotion (issue #155). Any `critical` finding for the scoped agent blocks promotion (fail-closed). The gate **does not consult or persist to `security_findings`** — it re-evaluates the live event corpus on every promotion attempt. Therefore triaging existing rows in `security_findings` will *not* unblock the gate; you must remove or age out the underlying data that the scan flags (or disable the gate). |
| `sentinel.promotion_gate_timeout_secs` | u64 | `30` | Maximum seconds the gate waits for the in-memory sweep before fail-closing. |
| `sentinel.sentinel_revision_id` | string | `"sentinel.current"` | Revision label embedded in findings produced by the live sentinel. Update when the sentinel logic changes to make findings filterable by version. |
| `sentinel.baseline_revision_id` | string | `"sentinel.baseline.frozen"` | Revision label for the frozen-baseline pass of the dual sweep. Phase-1 disagreements between baseline and current are recorded in `security_sentinel_disagreements`. |

**Promotion-gate semantics.** The gate is a fresh in-memory Phase-1 scan **scoped to the agent being promoted**, not a query against the persisted `security_findings` table. On every promotion attempt it scans the live causal-event / promotion-history / approval / sandbox-escape / layer-mount data filtered by the agent's ID (subject to the same `scan_limit` and `window_days` defaults as scheduled sweeps), counts critical findings, and blocks if the count is non-zero or if any check errored. It does not write findings; persisted rows in `security_findings` from earlier scheduled sweeps are not consulted.

Per-agent scope (issue #155, fixed). Each Phase-1 check applies the scope to the most relevant agent attribution column:

| Check | Scope column |
|---|---|
| `scan_credential_leaks` | `causal_events.agent_id` |
| `scan_capability_accretion` | `promotion_history.agent_id` |
| `scan_approval_denials`, `scan_exec_without_grant` | `approvals.agent_id` |
| `scan_escape_attempt_records` | `sandbox_escape_attempts.agent_id` |
| `scan_escape_patterns_in_events` | `causal_events.agent_id` |
| `scan_layer_scope_violations`, `scan_layer_provenance_gaps` | `approvals.agent_id` (the *mounting* agent) |

Consequence: a critical finding for agent A no longer blocks promotion of agent B. Cross-agent isolation is verified by the `promotion_gate_does_not_block_unrelated_agent`, `promotion_gate_blocks_same_agent_after_unrelated_findings`, and `promotion_gate_layer_mount_finding_anchors_to_mounting_agent` integration tests.

If the gate blocks for the agent being promoted, the unblock paths remain: (a) resolve the underlying signal in the data the scan reads, (b) wait for the windowed checks to age past their cutoff, (c) raise per-check thresholds in the gate's `SweepConfig`, or (d) set `promotion_gate_enabled: false`.

Example:

```yaml
sentinel:
  enabled: true
  full_sweep_schedule: "0 3 * * *"
  incremental_sweep_schedule: "0 */6 * * *"
  promotion_gate_enabled: true
  promotion_gate_timeout_secs: 30
  sentinel_revision_id: "sentinel.current"
  baseline_revision_id: "sentinel.baseline.frozen"
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
| `hooks[].allowed_agents` | list | `[]` | ACL for `agent.spawn` hooks: restricts which agent IDs may be spawned. Empty list = any agent allowed. |
| `hooks[].callback_allowlist` | list | `[]` | Required for `http.callback`. Allowlist entries use grant-target shapes such as `{ kind: "url_prefix", value: "https://hooks.example.com/autonoetic/" }` |

### agent.spawn hook parameters

| Parameter | Description |
|-----------|-------------|
| `params.agent_id` | Target agent to spawn (required). |
| `params.message_template` | Message with `{{field}}` substitution from hook context. Always includes `{{event}}`. Event-specific fields: `request_id`, `decision` (`approval.resolved`); `close_reason`, `turn_count` (`session.closed`); `workflow_id`, `task_ids` (`workflow.join.satisfied`); `root_session_id`, `session_id`, `agent_id`, `event_id`, `rule_ids`, `primary_rule_id`, `status`, `category`, `action`, `target`, `reason`, `turn_id`, `source` (`policy.decision`). |

`agent.spawn` hooks must set `async: true` — synchronous spawn is not supported.

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

> **Note:** With the auto-learning pipeline (see below), `digest_agent.enabled` defaults to `true`.

---

## Context Compression

Summarizes old conversation turns when approaching context limits. The hierarchical capsule strategy is the default LLM-tier reducer in the context governor pipeline.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `context_compression.enabled` | bool | `true` | Enable context compression. Requires a resolvable `llm_preset` or inline `provider`/`model`; otherwise the capsule strategy logs a warning and skips compression for the turn. |
| `context_compression.llm_preset` | string | `null` | LLM preset name for compression (should be a cheap/fixed model, not a routing preset). |
| `context_compression.provider` | string | `null` | Inline provider when `llm_preset` is not set. |
| `context_compression.model` | string | `null` | Inline model when `llm_preset` is not set. |
| `context_compression.threshold_pct` | float | `60.0` | Compress when conversation tokens exceed this percentage of the context window. |
| `context_compression.recent_turns_to_keep` | usize | `3` | Recent turns kept in full (not summarized). |
| `context_compression.max_summary_tokens` | usize | `500` | Maximum compressed summary size in tokens. |
| `context_compression.min_turns_between_compression` | u64 | `3` | Minimum turns between compression operations (prevents thrashing). |
| `context_compression.max_capsule_decisions` | usize | `30` | Capsule decisions retained before summarization (capsule strategy only). |
| `context_compression.max_completed_tasks` | usize | `10` | Completed capsule tasks retained (capsule strategy only). |

Example:

```yaml
context_compression:
  enabled: true
  llm_preset: haiku
  threshold_pct: 60.0
  recent_turns_to_keep: 3
  max_summary_tokens: 500
  min_turns_between_compression: 3
  max_capsule_decisions: 30
  max_completed_tasks: 10
```

---

## Scheduled Jobs (Cron)

Controls cron-style scheduled job admission and dispatch.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `scheduled_jobs.min_interval_secs` | u64 | `1` | Minimum interval between job triggers (seconds). |
| `scheduled_jobs.max_per_root` | usize | `50` | Max scheduled jobs per root session. |
| `scheduled_jobs.max_due_per_tick` | usize | `16` | Max due jobs processed per canonical scheduler tick. |

> Sub-10s schedules are allowed only for script-mode target agents. For tight schedules (&lt;5s), reduce `background_tick_secs` to `1` for better precision.

---

## Auto-Learning Pipeline

Controls the default self-improvement loop. When enabled, the gateway automatically distills memories after sessions, emits quality signals, and schedules periodic memory curation.

When `auto_learning.enabled` is `true`, startup injects synthetic cron rows for:

- `memory-curator.default`, driven by `auto_learning.curation_schedule`
- `evolution-orchestrator.default`, on a fixed daily cadence (`0 3 * * *`)

Injection is skipped when an enabled `system_agents` entry already declares a schedule for those targets, or when the agent bundle is missing from `agents_dir`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `auto_learning.enabled` | bool | `true` | Master switch for the auto-learning pipeline. |
| `auto_learning.quality_signals` | bool | `true` | Emit per-session quality signals (turn count, error count, completion) as Tier-2 memories tagged `source:quality_signal`. |
| `auto_learning.curation_schedule` | string | `"0 */4 * * *"` | Cron expression forwarded to the injected `memory-curator.default` job (UTC). Ignored when auto-learning is disabled or the curator already has an active system cron. |

```yaml
auto_learning:
  enabled: true
  quality_signals: true
  curation_schedule: "0 */4 * * *"
```

To opt out entirely:

```yaml
auto_learning:
  enabled: false
```

---

## Profiles

Complexity profiles control default behaviors and visibility. Explicit config overrides always win over profile defaults.

| Profile | Description |
|---------|-------------|
| `starter` | Simplified UX. Auto-approves safe tool invocations, simplified help text, auto-learning ON, generous memory priming (3 memories). |
| `standard` | Current behavior (default). All approvals require operator confirmation. Memory priming limit: 5. |
| `expert` | Full constitutional visibility. Rule IDs shown alongside approval cards. Memory priming limit: 10. |

```yaml
profile: starter
```

---

## User Persona

A Markdown file loaded at gateway start and injected into every agent's system prompt between the constitutional foundation and agent-specific instructions. Enables cross-agent personalization (communication style, domain context, preferences).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `persona_path` | string (path) \| null | `null` | Path to persona file. Relative paths resolve from config directory. When `null`, looks for `persona.md` next to config file. |

The persona file is free-form Markdown:

```markdown
I'm a senior Rust developer working on distributed systems.
Respond concisely. Use English for code, French for prose.
My project uses tokio + axum + SQLite.
```

Set or view the persona from the chat TUI with `/persona [text]`.

The persona layer sits after constitutional rules (cannot override them) and before agent-specific instructions (agents can further specialize).

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
http_port: 4100
ofp_port: 4200
tls: false
node_id: "gateway"
node_name: "gateway"
constitution:
  source_path: "docs/constitution/versions/2026.05.19/constitution.md"
  lock_path: "docs/constitution/versions/2026.05.19/gateway-constitution.lock.json"
  require_signature: true
  trusted_signers:
    autonoetic:constitution:v1: "lNxT1b/jWa6LqM2Thd7rW1IppvlH3rlEnAOPV81Igzk="
max_concurrent_spawns: 8
max_pending_spawns_per_agent: 4
max_spawn_depth: 8
approval_timeout_secs: 600
max_pending_approvals_per_root: 50
# continuation_key: "set-me-in-production-from-a-secret-source"
workflow_task_heartbeat_secs: 2
stuck_task_timeout_secs: 600
approval_dwell_multiplier: 1.0
signal_delivery_timeout_secs: 60
evidence_mode: full
max_session_turns: 12
capability_delta_gate_mode: strict
interaction_answer_orchestration: true
allow_runtime_lock_drift: false
trust_unsigned_bundles: false

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

loop_guard:
  max_loops_without_progress: 5
  max_tool_failures: 5
  max_consecutive_same_progress: 1
  max_child_failures: 3

prompt_budget:
  warn_at_pct: 80.0
  margin_tokens: 4096

retention:
  execution_traces_days: 30
  causal_events_days: 90

digest_agent:
  enabled: true
  min_turns: 2
  llm_preset: agentic

context_compression:
  enabled: true
  llm_preset: haiku
  threshold_pct: 60.0
  recent_turns_to_keep: 3
  max_summary_tokens: 500
  min_turns_between_compression: 3

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
