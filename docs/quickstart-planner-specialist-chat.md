# Quickstart: Planner to Specialist Chat

This quickstart verifies the full routing flow:

1. terminal chat ingress with an explicit `target_agent_id: "planner.default"`
2. gateway resolves target through alias registry to the active revision
3. planner delegates to a specialist via `agent.spawn`
4. specialist result returns in the same session

It also includes the required config-file step so `agent bootstrap` does not fall back to unintended defaults.

## Prerequisites

- workspace root available
- Rust toolchain installed
- At least one LLM provider API key (see environment variables below)

## Environment Variables

All secrets and provider keys are kept **out of config.yaml** and passed as env vars.

### Required

| Env var | Purpose |
|---------|---------|
| `AUTONOETIC_SHARED_SECRET` | Gateway auth token — must be set before `gateway start`. Not stored in config. |
| _One or more provider API keys_ | At least the key for the provider referenced in your `llm_presets` (see table below). |

### LLM Provider API Keys

Each LLM provider reads its key from a standard env var. Export the ones your `llm_presets` reference:

| Provider | Env var |
|----------|---------|
| OpenRouter | `OPENROUTER_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Anthropic | `ANTHROPIC_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| Together | `TOGETHER_API_KEY` |
| Fireworks | `FIREWORKS_API_KEY` |
| xAI | `XAI_API_KEY` |
| Perplexity | `PERPLEXITY_API_KEY` |
| Cohere | `COHERE_API_KEY` |
| Cerebras | `CEREBRAS_API_KEY` |
| SambaNova | `SAMBANOVA_API_KEY` |
| HuggingFace | `HUGGINGFACE_API_KEY` |
| Replicate | `REPLICATE_API_TOKEN` |
| Moonshot/Kimi | `MOONSHOT_API_KEY` |
| Qwen/DashScope | `DASHSCOPE_API_KEY` |
| Ollama / vLLM / LM Studio | _(none — local providers)_ |

A global override `AUTONOETIC_LLM_API_KEY` exists but is not recommended when using provider-specific keys. It is ignored unless `AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES=true`.

### Optional

| Env var | Default | Purpose |
|---------|---------|---------|
| `AUTONOETIC_NODE_ID` | config `node_id` or `"gateway"` | Node identity for OFP federation and causal chain authorship |
| `AUTONOETIC_NODE_NAME` | config `node_name` or `"gateway"` | Human-readable node name |
| `AUTONOETIC_BWRAP_SHARE_NET` | config `sandbox.share_net` or `false` | Share host network namespace (ignored unless `AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true`) |
| `AUTONOETIC_BWRAP_DEV_MODE` | config `sandbox.dev_mode` or `"legacy"` | `/dev` mount strategy for bubblewrap (ignored unless `AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=true`) |
| `AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES` | `false` | Allow `AUTONOETIC_BWRAP_*` env overrides to bypass config |
| `AUTONOETIC_ALLOW_LLM_ENV_OVERRIDES` | `false` | Allow `AUTONOETIC_LLM_BASE_URL` / `AUTONOETIC_LLM_API_KEY` env overrides |
| `AUTONOETIC_EVIDENCE_MODE` | config `evidence_mode` or `"full"` | How much tool/LLM data to save (`full`, `errors`, `off`) |
| `AUTONOETIC_VAULT_PATH` | — | Vault file location (for credential management) |
| `AUTONOETIC_VAULT_KEY` | — | Hex-encoded 32-byte AES-256-GCM master key for vault encryption |
| `AUTONOETIC_VAULT_KEY_PATH` | — | Path to file containing hex vault key (alternative to inline key) |
| `AUTONOETIC_LLM_CONTEXT_WINDOW` | — | Override context window for prompt budget enforcement |
| `AUTONOETIC_GOOGLE_SEARCH_API_KEY` | — | Google Custom Search API key for `web.search` |
| `AUTONOETIC_GOOGLE_SEARCH_ENGINE_ID` | — | Google Custom Search Engine ID |
| `AUTONOETIC_HOST_ID` | hostname | Host/process identity for active execution tracking |
| `AUTONOETIC_MCP_REGISTRY_PATH` | — | Override MCP server registry file path |
| `AUTONOETIC_REFERENCE_AGENTS_DIR` | — | Override reference bundles location for `agent bootstrap` |
| `AUTONOETIC_OPENROUTER_CATALOG` | `1` | Set to `0` to disable OpenRouter model catalog |
| `AUTONOETIC_LLM_OTHER_EMPTY_RETRIES` | `0` | Retry count when LLM returns empty response |

Node identity env vars can override config values. Security-sensitive sandbox and global LLM env overrides are blocked by default unless explicitly enabled via `AUTONOETIC_ALLOW_*_ENV_OVERRIDES=true`.

## 1) Create config (quick method)

Use `agent init-config` to generate a config with LLM presets:

```bash
mkdir -p /tmp/autonoetic-demo
cargo run -p autonoetic -- agent init-config --output /tmp/autonoetic-demo/config.yaml --overwrite
```

This creates a config with:
- Gateway settings (ports, limits, scheduler)
- Response validation with repair enabled
- Schema enforcement, code analysis, retention
- Prompt budget transparency (observability + enforcement)
- LLM presets (agentic, coding, research, fallback)
- Template-to-preset mappings for automatic LLM selection

See `docs/config-reference.md` for the full configuration reference.

### Alternative: Manual config

```bash
mkdir -p /tmp/autonoetic-demo
cat > /tmp/autonoetic-demo/config.yaml <<'EOF'
agents_dir: "/tmp/autonoetic-demo/agents"
port: 4000
ofp_port: 4200
tls: false
node_id: "demo"
node_name: "demo"
max_concurrent_spawns: 8
max_pending_spawns_per_agent: 4

sandbox:
  share_net: true
  dev_mode: host-bind

background_scheduler_enabled: true
background_tick_secs: 5
background_min_interval_secs: 60
max_background_due_per_tick: 32

response_validation:
  enabled: true
  repair_enabled: true

approval_timeout_secs: 600

schema_enforcement:
  mode: deterministic
  audit: true

evidence_mode: full

code_analysis:
  capability_provider: pattern
  security_provider: pattern
  require_capabilities: true
  require_approval_for:
    - NetworkAccess
    - CodeExecution

retention:
  execution_traces_days: 30
  causal_events_days: 90

# ── Prompt Budget ─────────────────────────────────────────────────────
# Controls token budget observability and enforcement per LLM request.
# See docs/prompt-budget.md for details.
prompt_budget:
  system_prompt_max_tokens: 0      # 0 = unlimited
  tool_definitions_max_tokens: 0   # 0 = unlimited
  warn_at_pct: 80.0                # warn when utilization exceeds this %
  margin_tokens: 4096              # reserve for LLM output
  on_exceeded: warn                # warn | trim_history | demote_tools | fail
  compress_tool_schemas_after_turn_0: false

# ── Session Budget (optional per-session resource limits) ─────────────
# Uncomment to cap LLM rounds, tool calls, tokens, or wall-clock time.
# session_budget:
#   profile: dev
#   max_llm_rounds: 200
#   max_tool_invocations: 500
#   max_llm_tokens: 5000000
#   max_wall_clock_secs: 3600

# LLM presets for role-specific model selection
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

# Template → Preset mapping
llm_preset_mapping:
  planner: agentic
  researcher: research
  architect: agentic
  coder: coding
  debugger: coding
  auditor: agentic
  evaluator: agentic
  specialized_  packager: agentic
  default: agentic
EOF
```

## 2) Bootstrap reference bundles and activate agents

From `autonoetic/`:

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent bootstrap
```

Bootstrap performs three steps for each reference agent bundle:
1. Copies reference bundle files into the runtime `agents_dir`
2. Creates an **immutable revision** in the GatewayStore (content-hashed and deduplicated)
3. Creates an **alias binding** that points to the new revision (activation)

After bootstrap, the alias registry is authoritative for agent resolution — runtime execution resolves through alias → revision, not directory scanning.

Bootstrap automatically applies LLM presets from config:
- If `llm_preset_mapping` exists, each template uses its mapped preset
- If no mapping, templates use role-specific defaults (planner → agentic, coder → coding, etc.)
- Override with `--preset` flag when creating individual agents

Optional:

```bash
# Force replacement of existing runtime agent dirs
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent bootstrap --overwrite

# Use an explicit bundle source directory
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent bootstrap --from /path/to/autonoetic/agents
```

### 2a) Verify alias bindings

After bootstrap, verify that all agents are activated with revision bindings:

```bash
# List all alias bindings and their active revisions
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent alias list
```

You should see output like:

```
ALIAS ID                     AGENT ID                     ACTIVE REVISION                STATUS     UPDATED AT
planner.default              planner.default              rev_a1b2c3d4                   Ready      2026-04-02T...
researcher.default           researcher.default           rev_e5f6a7b8                   Ready      2026-04-02T...
coder.default                coder.default                rev_c9d0e1f2                   Ready      2026-04-02T...
...
```

### 2b) Check LLM presets

```bash
# List configured presets and template mappings
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent presets
```

This shows available presets and which template uses which preset.

### 2c) Researcher and web search (required for "search today's weather" etc.)

The researcher can use native `web.search` and `web.fetch` only if its runtime SKILL has a **NetworkAccess** capability that allows the target hosts (e.g. DuckDuckGo, or `*` for all).

- If you see errors when the researcher runs goals like "search today's weather", the runtime researcher may have been created from an older bundle without NetworkAccess. Re-bootstrap so it gets the current researcher (with `NetworkAccess` and `hosts: ["*"]`):

  ```bash
  cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent bootstrap --overwrite
  ```

- To confirm the runtime researcher can use web search, check that its SKILL includes NetworkAccess:

  ```bash
  grep -A1 "NetworkAccess" /tmp/autonoetic-demo/agents/researcher.default/SKILL.md
  ```

  You should see `hosts: ["*"]` (or at least hosts that include `duckduckgo.com` and any other search/fetch targets).

- **If NetworkAccess is present but the researcher still doesn't use web search** (e.g. for "search today's weather"): the model may be answering from training data instead of calling the tool. Re-bootstrap so the researcher gets the latest instructions (which tell it to always call `web.search` first for current/live info), then restart the gateway and try again. You can also inspect the planner/researcher trace to see whether `web.search` was in the tool list and whether the model requested it:

  ```bash
  cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace show demo-session --agent researcher.default
  ```

### 2d) Optional: add MCP web tools (native web tools already available)

You can still add MCP web tools for richer provider-specific search/fetch behavior.

To enable Google provider in native `web.search`, export:

```bash
export AUTONOETIC_GOOGLE_SEARCH_API_KEY="..."
export AUTONOETIC_GOOGLE_SEARCH_ENGINE_ID="..."
```

Legacy aliases are also accepted for compatibility:

```bash
export GOOGLE_SEARCH_API_KEY="..."
export GOOGLE_SEARCH_ENGINE_ID="..."  # or GOOGLE_SEARCH_CX
```

Then the researcher can call `web.search` with either explicit Google or auto fallback:

```json
{ "query": "rust async runtime", "provider": "google" }
```

```json
{
  "query": "rust async runtime",
  "provider": "auto",
  "cache_ttl_secs": 120
}
```

`provider: "auto"` tries Google first when credentials are available, then falls back to DuckDuckGo on missing credentials, errors, or empty Google results.
`cache_ttl_secs` controls in-memory response caching (0 disables cache, max 3600 seconds).

**If `web.search` returns `result_count: 0`** (e.g. for "weather in Paris"): DuckDuckGo's API often returns no results for weather and similar instant-answer queries. The tool call still succeeds (`ok: true`); the researcher just gets an empty result set. For better coverage on live/weather queries, set up the Google provider (see above) and use `provider: "auto"` or `"google"` so the researcher can use Google Custom Search when available.

Additional `web.search` options for advanced setups:

- `duckduckgo_engine_url`: override DuckDuckGo endpoint for local/mock engines.
- `google_engine_url`: override Google endpoint for local/mock engines.
- `google_api_key_env`: env var name for API key (default `AUTONOETIC_GOOGLE_SEARCH_API_KEY`).
- `google_engine_id_env`: env var name for Custom Search Engine ID (default `AUTONOETIC_GOOGLE_SEARCH_ENGINE_ID`).

Response metadata now includes:

- `requested_provider`: provider asked by caller (`auto`, `google`, or `duckduckgo`).
- `attempted_providers`: providers tried in execution order.
- `fallback_reason`: why fallback occurred (present when fallback is used).
- `cache_hit`: whether response came from cache.

Example registration:

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  mcp add web --command /path/to/your-web-mcp-server -- --stdio
```

Then verify MCP availability:

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway status
```

If you want stricter network policy, narrow `researcher.default` `NetConnect.hosts` in your runtime agent bundle instead of using `["*"]`.

## 3) Create new agents with LLM presets

After bootstrap, create additional agents with specific LLMs:

```bash
# Using preset name from config
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  agent init weather_agent --template coder --preset coding

# Using direct provider/model override
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  agent init search_agent --template researcher \
  --provider anthropic --model claude-sonnet-4-20250514

# List available presets
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent presets
```

### Agent Template Defaults

When no preset is specified, each template uses a role-optimized default:

| Template | Default Provider | Default Model | Why |
|----------|-----------------|---------------|-----|
| planner | anthropic | claude-sonnet-4-20250514 | Best agentic/tool-use capabilities |
| researcher | openai | gpt-4o | Strong research and synthesis |
| coder | anthropic | claude-sonnet-4-20250514 | Best code generation |
| evaluator/auditor | openrouter | google/gemini-3-flash-preview | Cost-efficient analysis |
| generic | openai | gpt-4o | Balanced capabilities |

## 4) Start gateway

From `autonoetic/`:

```bash
AUTONOETIC_SHARED_SECRET=demo-secret \
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway start
```

The only required environment variable is `AUTONOETIC_SHARED_SECRET` (kept out of config.yaml for security). Node identity and sandbox settings are read from `config.yaml` by default.

If you previously exported overrides in your shell, clear them before starting the gateway:

```bash
unset AUTONOETIC_LLM_API_KEY AUTONOETIC_LLM_BASE_URL
```

### Sandbox compatibility

The `sandbox` section in config.yaml handles environments where `bwrap --unshare-net` cannot configure loopback or where `/dev/null` writes fail. The quickstart config already sets `share_net: true` and `dev_mode: host-bind`. If you still need env overrides for one-off debugging, explicitly opt in:

```bash
AUTONOETIC_SHARED_SECRET=demo-secret \
AUTONOETIC_ALLOW_SANDBOX_ENV_OVERRIDES=1 \
AUTONOETIC_BWRAP_SHARE_NET=1 \
AUTONOETIC_BWRAP_DEV_MODE=host-bind \
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway start
```

## 5) Open terminal chat with implicit routing

In a second terminal, from `autonoetic/`:

```bash
AUTONOETIC_SHARED_SECRET=demo-secret \
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml chat --session-id demo-session
```

Do not pass an `agent_id`. This exercises implicit routing to the session/default lead.

## 6) Trigger delegation

In chat, send a request that should require specialist work, for example:

```text
Research Rust JSON-RPC libraries and summarize tradeoffs.
```

Expected behavior:

- gateway ingress resolves to `planner.default` via alias registry → pinned revision
- planner uses `agent.spawn` to call an appropriate specialist (for example `researcher.default`)
- planner synthesizes and returns response

## 7) Memory and state model (current)

Current runtime behavior is a hybrid:

- Tier 1 local state lives under each agent directory (`state/`) and is suitable for deterministic, near-term continuity.
- Tier 2 durable memory is gateway-managed (`memory.db`) and should be used for reusable/cross-session facts.
- Gateway injects compact session context for same-session continuity; this is not yet a full automatic `state/summary.md` pipeline.
- **Session transcripts** are automatically persisted at hibernation and session close and indexed with SQLite FTS5 for full-text search. Agents can search past sessions with `session.search` and summarize them with `session.peek` (see `docs/fts-session-search.md`).

For multi-step tasks that benefit from explicit textual state, prefer these conventions:

- `state/task.md` -> active checklist and next action.
- `state/scratchpad.md` -> short-lived notes/intermediate reasoning.
- `state/handoff.md` -> concise progress/blockers/next-step handoff.

## 8) Verify traces

**Where to look:**

- **Gateway causal chain** — `agents/.gateway/history/causal_chain.jsonl` — records every ingress (top-level `event.ingest` when you chat) and every **delegation** (each `agent.spawn` from planner → researcher, coder, etc.). One place to see the full delegation tree for a session.
- **Per-agent causal chains** — `agents/<agent_id>/history/causal_chain.jsonl` — record that agent's lifecycle, LLM calls, and tool invocations (including `agent.spawn` requests and results as seen by that agent).

```bash
# Gateway log (all delegations for the session)
cat /tmp/autonoetic-demo/agents/.gateway/history/causal_chain.jsonl

# Per-agent traces
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace sessions --agent planner.default
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace show demo-session --agent planner.default
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace sessions --agent researcher.default
```

You should see:

- gateway log: `event.ingest.requested` / `event.ingest.completed` for the chat, then `agent.spawn.requested` / `agent.spawn.completed` for each delegation (researcher, architect, coder, etc.);
- planner session activity for `demo-session`;
- tool usage including `agent.spawn` in planner trace;
- specialist session activity tied to the same request lineage.

**Human-readable session views:**

- `agents/.gateway/sessions/<session_id>/timeline.md` — progressive Markdown timeline for the whole session (includes mirrored `workflow.*` gateway rows when delegation uses the durable workflow store).
- `agents/.gateway/sessions/<session_id>/workflow_graph.md` — rewritten whenever a workflow event appends: current `workflow_id`, task list, and recent workflow store events (open beside `timeline.md` for a live orchestration snapshot).
- `agents/.gateway/sessions/<session_id>/artifacts/<artifact_id>/` — named projection of built artifact files so you can open generated code directly without resolving SHA handles by hand.
- failed or approval-blocked tool runs now attach an `evidence_ref` in the timeline/causal entry, pointing to the full redacted result payload (useful for test stdout/stderr and approval details).

**Additional trace commands:**

```bash
# Follow session events in real-time
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace follow demo-session

# Show durable workflow orchestration events
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace workflow demo-session --root

# Show workflow graph (text DAG visualization)
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace graph demo-session

# Show conversation history for a session
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace history demo-session

# Fork a session from a snapshot to explore alternative paths
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace fork demo-session --message "Try a different approach" --interactive

# Print the post-session narrative digest
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml trace digest demo-session
```

**Why is `result_preview` truncated in causal_chain.jsonl?**  
Tool results in the causal chain are intentionally limited to 256 characters so log lines stay readable and bounded. The payload still has `result_len` and `result_sha256`. By default, the gateway now captures full redacted evidence and adds an `evidence_ref` for traced events under the agent's `history/evidence/<session_id>/`. If you want to reduce evidence volume, set `AUTONOETIC_EVIDENCE_MODE=off`; failed and approval-blocked tool runs will still preserve an `evidence_ref` so the full error/test payload remains inspectable.

**Does `causal_chain.jsonl` rotate?**  
Not yet. Current logs append to a single file per history location (`agents/.gateway/history/causal_chain.jsonl` and `agents/<agent_id>/history/causal_chain.jsonl`). Rotation/segmentation is planned.

## Adapter specialist docs

For schema/behavior wrapper generation via `agent-adapter.default`, including
details of `schema_diff.py` and `generate_wrapper.py`, see:

- `docs/agent-adapter-specialist.md`

## Agent Activation Model (revision + promote)

Agent activation uses an **immutable revision** model:

1. **Create artifact** — agent source files are bundled into a content-addressed `AgentBundle` artifact
2. **Create revision** — `agent.revision.create` or `agent revision create` produces an immutable revision with content digest, runtime closure, and status (`candidate` or `ready`)
3. **Promote** — `agent.revision.promote` or `agent revision promote` moves the alias to point to the new revision
4. **Sessions pin** — when a session starts, a session binding records the exact revision and runtime lock hash; running sessions are unaffected by later promotions

The `agent bootstrap` command performs all three steps automatically. For manual lifecycle:

```bash
# Create a revision from an artifact
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  agent revision create planner.default <artifact_id>

# Promote a revision (moves alias target)
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  agent revision promote planner.default <revision_id> --reason "Tested and verified"

# Inspect promotion history
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  agent promotion-history --agent-id planner.default

# Deterministic seed (useful for tests)
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  agent seed planner.default <revision_id>
```

## Credential Management (optional)

Agents can securely interact with external APIs without secrets leaking into the LLM context. The gateway handles secret storage, injection, and redaction.

### Setup vault encryption

```bash
# Generate a vault key (do this once)
openssl rand -hex 32 > /tmp/autonoetic-demo/vault.key

# Set env vars before starting gateway
export AUTONOETIC_VAULT_PATH=/tmp/autonoetic-demo/vault.dat
export AUTONOETIC_VAULT_KEY_PATH=/tmp/autonoetic-demo/vault.key
```

### Credential lifecycle

1. Agent calls `credential.check("github")` → sees if a credential exists (no secret exposed)
2. Agent calls `credential.setup(...)` → may suspend for human approval if `user_prompt` step
3. Operator approves via TUI or CLI (secrets entered via masked prompt or `--secret` flag)
4. Agent calls `credential.request(...)` → gateway injects secret into HTTP request, returns redacted response

```bash
# Interactive TUI for approving credential prompts (recommended — masked input)
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  gateway approvals interactive

# Non-interactive approval with secret
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  gateway approvals approve apr-XXXXXXXX --secret github_token=ghp_xxxx
```

See `docs/credential-management.md` for details.

## Importing External Skills (AgentSkills.io)

Import external skills from the agentskills.io ecosystem:

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml \
  agent import-skill --from /path/to/external-skill \
  --agent-id myagent.default --trust strict
```

Trust modes: `generous` (auto-grant), `strict` (approval per capability), `audit` (dry-run sandbox).

## Common Pitfall

If `--config` points to a missing file, bootstrap now fails fast by design.

Fix:

1. create the config file first (step 1)
2. rerun bootstrap

If planner installs ad-hoc agents like `researcher` or returns unvalidated code (instead of routing to `*.default` specialists with evaluator checks), your runtime state likely drifted.

Fix:

1. stop gateway/chat processes for this config
2. remove drifted runtime agents (for example `/tmp/autonoetic-demo/agents/researcher` and generated throwaway agents)
3. re-bootstrap with overwrite
4. restart gateway and use a new session id

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent bootstrap --overwrite
```

Then verify only canonical specialist IDs are present before testing:

```bash
ls -1 /tmp/autonoetic-demo/agents

# Or check alias bindings (authoritative source of truth)
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml agent alias list
```

## Approvals (revision promotion, credentials, and scheduled actions)

When a privileged operation such as `agent.revision.promote` or a credential setup triggers an approval gate, the tool returns `approval_required: true` and a `request_id` (short ID format like `apr-db51b7ad`). The operation does not proceed until an operator approves.

**List pending approval requests:**

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway approvals list
```

**Approve or reject a request:**

```bash
# Approve — gateway auto-resumes the suspended turn
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway approvals approve apr-db51b7ad --reason "Reviewed; OK to promote"

# Approve credential prompt with secret values
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway approvals approve apr-db51b7ad --secret api_token=sk-xxx

# Reject
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway approvals reject apr-db51b7ad --reason "Out of scope"
```

**Interactive TUI (recommended for credential prompts):**

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway approvals interactive
```

The interactive TUI shows pending requests, allows review, and uses masked password prompts for credential secrets — avoiding shell history exposure.

**Execution and notification flow (recommended):**
1. Tool returns `approval_required: true` with `request_id: "apr-db51b7ad"`
2. Turn is suspended to disk (turn continuation)
3. Operator approves via CLI or interactive TUI
4. **Gateway automatically resumes the turn** and executes the approved action with real tool results
5. Gateway persists an approval-resolution notification for the waiting session
6. If terminal chat is open on that session, chat resumes automatically and displays the continuation
7. If no consumer is connected, notification remains pending until acknowledged

You should not need to type manual prompts like `continue` or `done` after approval in the normal chat path.

**Delivery semantics (current model):**
- Approval-resolution messages use a structured payload (`type: "approval_resolved"` with `request_id`, `status`, `message`).
- The gateway owns background polling/delivery; CLI approval commands only record the decision.
- Chat acknowledges notification consumption only after successful resume/render.
- Pending notifications are durable in the `GatewayStore` SQLite database until consumed.

**Rejected requests** are not retried; the caller sees the rejection and should report to the user.

**LLM truncation note:** Short approval IDs (`apr-XXXXXXXX`) are used to avoid truncation bugs in some LLMs (e.g., Gemini 3 Flash truncates UUIDs by one character).

**Machine-readable list:** Use `--json` for JSON output:

```bash
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway approvals list --json
```
For architecture details, see `docs/approval-notification-delivery.md`.

## User Interactions

The gateway supports a structured user-interaction channel separate from approvals, for cases where agents need clarifying information from the user:

```bash
# List pending interactions
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway interactions list

# Answer an interaction
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway interactions answer <id> --text "Yes, use the production database"

# Cancel an interaction
cargo run -p autonoetic -- --config /tmp/autonoetic-demo/config.yaml gateway interactions cancel <id> --reason "No longer needed"
```

## Troubleshooting: capability errors and memory writes

**"memory write denied by policy" / "scheduled file write denied by WriteAccess policy"**

- The agent has a `WriteAccess` capability with `scopes` that do not cover the path being written to.
- **Fix:** Add a `WriteAccess` with `scopes` that cover the path, e.g. `["skills/*", "state/*"]`. Paths must be under the agent directory; do not use absolute or `..` paths. Prefer putting files under `skills/*` (e.g. `skills/helper.md`, `skills/script.py`) so they are clearly in scope.

**"AgentRevision capability required" / revision tool errors**

- The agent calling `agent.revision.create` or `agent.revision.promote` does not have the `AgentRevision` capability.
- **Fix:** Only `specialized_builder.default` (or `evolution-steward.default`) should call revision tools. Ensure the correct agent is being delegated to. The capability must declare `patterns` matching the target agent ID.

## Shell Execution Safety Policy (sandbox.exec)

Some specialists can execute shell via `sandbox.exec` (typically through `bash -c` or `sh -c`).

| Class | Examples | Policy |
|---|---|---|
| Safe deterministic shell glue | `bash -c 'pytest -q'`, `bash -c 'ls src'`, `bash -c 'cat report.txt'` | Allowed if agent `CodeExecution` patterns permit it |
| Destructive filesystem operations | `rm`, `rmdir`, `unlink`, `shred`, `wipefs`, `mkfs`, `dd`, `find ... -delete` | Hard deny |
| Privilege escalation | `sudo`, `su`, `doas`, setuid/setgid patterns | Hard deny |
| Environment/process disclosure | `env`, `printenv`, `declare -x`, `/proc/*/environ` reads | Hard deny |

If a command matches an agent's `CodeExecution` pattern but still fails with permission/security errors, assume the command hit one of these hard boundaries and rewrite the approach.

**Networking and `/dev` troubleshooting with bubblewrap**

- `bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted` means the host/kernel blocks loopback setup in isolated net namespaces. Use `AUTONOETIC_BWRAP_SHARE_NET=1` for that environment.
- `curl` reporting `HTTP:200` together with `Failure writing output to destination` means the request succeeded but output write failed (often `/dev/null` or destination path). Use writable paths (for example `/tmp/...`) and, if needed, `AUTONOETIC_BWRAP_DEV_MODE=host-bind` or `minimal`.

## Related Docs

- `docs/config-reference.md` — full `config.yaml` field reference
- `docs/ARCHITECTURE.md` — system design and data flow
- `docs/AGENTS.md` — agent roles, routing, SKILL.md format
- `docs/response-validation-gate.md` — validation and repair internals
- `docs/session-budget.md` — per-session resource limits
- `docs/code-analysis.md` — capability and security analysis
- `docs/CLI.md` — CLI command reference
- `docs/prompt-budget.md` — prompt budget transparency and enforcement
- `docs/credential-management.md` — secure credential management
- `docs/fts-session-search.md` — FTS session search
- `docs/workflow-orchestration.md` — workflow orchestration
- `docs/agent-capabilities.md` — agent capabilities reference
- `docs/plan-agent-revision-evaluation-federation-mvp.md` — revision, evaluation, and federation plan
- `docs/plan-hermes-gap-closure.md` — Hermes gap closure plan
