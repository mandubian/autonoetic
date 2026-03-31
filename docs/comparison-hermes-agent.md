# Autonoetic vs Hermes-Agent Comparison

> Comparing the Autonoetic agent runtime (Rust) with Hermes-Agent (Python, by Nous Research).
> Last updated: 2026-03-31, based on code audit of both codebases.

---

## 1. Core Philosophy

| Dimension | Autonoetic | Hermes-Agent |
|---|---|---|
| **Paradigm** | Gateway-mediated separation of powers: agents are pure reasoners, gateway is sole executor | Direct agent loop: LLM calls tools directly, no intermediary gateway |
| **Language** | Rust-first, with Python/TypeScript SDKs | Python-first |
| **Governance model** | Formal capability-based access control with policy engine | Tool approval + command allowlists (lighter-weight) |
| **Primary focus** | Auditable, portable, reproducible agent execution | Practical, daily-use AI assistant with learning loops |

## 2. Agent Architecture

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Execution model** | Two modes: **Reasoning** (LLM loop) and **Script** (deterministic, no LLM) | Single synchronous loop: `LLM → tool_calls → execute → repeat` |
| **Agent definition** | `SKILL.md` YAML frontmatter + markdown instructions | Inline prompts, `SOUL.md` persona, `AGENTS.md` instructions |
| **Manifest format** | Formal YAML with typed capabilities, IO contracts, disclosure policies | Config-driven (config.yaml + .env) |
| **Sandboxing** | Bubblewrap, Docker, MicroVM enforced by gateway | Terminal backends: local, Docker, SSH, Modal, Daytona, Singularity |
| **Separation of powers** | **Strict**: agents cannot touch filesystem/network/secrets directly; all via gateway | **Relaxed**: tools run directly in agent context |

## 3. Memory & Learning

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Tier 1 (working)** | Content-addressable store (SHA-256), visibility model (private/session/global), artifacts for trust boundary | File-based working memory, conversation context |
| **Tier 2 (durable)** | Gateway-managed `knowledge.*` tools with provenance, scope, visibility | Procedural memory via **Skills** (autonomous creation from experience), `MEMORY.md`/`USER.md` |
| **Cross-session recall** | `execution.search`, `knowledge.search_by_tags`, `digest.query` — three native tools querying unified gateway DB | FTS5 session search with LLM summarization, Honcho dialectic user modeling |
| **Post-session memory extraction** | **Implemented**: LLM-driven digest agent extracts error lessons, decisions, approaches, facts, open items; stores as tagged Tier-2 memories (`post_session_digest.rs`, 397 lines) | Background memory review thread after sessions end (turn-count triggered) |
| **Self-improvement** | **Partially implemented**: evolution agents exist as static bundles; learning tools and background scheduler are fully wired; no autonomous closed-loop skill creation/improvement yet | **Closed learning loop**: skills self-improve during use, autonomous skill creation after complex tasks (5+ tool calls), periodic nudges to persist knowledge |
| **Audit trail** | Hash-chained JSONL causal chain (immutable, verifiable) | Trajectory saving (for RL training), session persistence |

## 4. Tooling & Extensibility

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Tool count** | Core set: `content.*`, `knowledge.*`, `agent.*`, `artifact.*`, `web.*`, `sandbox.*`, MCP tools | **40+ built-in tools**: file ops, browser, code execution, web search, voice, TTS, vision, delegation, etc. |
| **MCP support** | MCP client + server (registry, discovery, agent exposure) | MCP client integration (`tools/mcp_tool.py`) |
| **Tool registration** | Capability-declared in SKILL.md manifest, validated by policy engine | Central `tools/registry.py` with schema collection, dispatch, availability checking |
| **Extensibility pattern** | Install agents via `agent.install`, discovery via `agent.discover` | Create `tools/your_tool.py`, import in `model_tools.py`, add to `toolsets.py` |
| **Federation** | OpenFang Protocol (OFP) with HMAC handshake | Platform gateway: Telegram, Discord, Slack, WhatsApp, Signal |

## 5. Multi-Agent System

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Agent roles** | Formal role catalog: Lead (planner), Specialists (researcher, architect, coder, debugger, evaluator, auditor), Evolution (builder, curator, steward) | Delegation via `delegate_tool.py` (spawn isolated subagents) |
| **Routing** | Explicit target → session affinity → default lead (`planner.default`) | Task-based delegation, no formal routing hierarchy |
| **Agent spawning** | `agent.spawn` with structured metadata (role, expected outputs, parent goal) | `execute_code` + `delegate` for subagent creation |
| **Agent persistence** | Durable agent directories with `SKILL.md` + `runtime.lock` | Ephemeral subagents (session-scoped) |

## 6. Portability & Reproducibility

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Runtime closure** | `runtime.lock` pins exact dependency versions, SDK, gateway, sandbox backend, layer mounts | `requirements.txt` / `pyproject.toml` / `uv.lock` |
| **Export format** | **Cognitive Capsule**: portable bundle of agent + runtime closure | No formal export; migration via `hermes claw migrate` from OpenClaw |
| **Remote agents** | HTTP Content API with Bearer auth, SDK auto-detects local vs remote | Gateway mirrors for messaging platforms |

## 7. Security Model

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Access control** | Capability-based: `ReadAccess`, `WriteAccess`, `SandboxFunctions`, `CodeExecution`, `AgentSpawn`, `NetworkAccess`, etc. with pattern scoping | Command approval detection, DM pairing, container isolation |
| **Secret handling** | Vault injection (never exposed to agent, zeroized after use) | `.env` file, `hermes config set`, secrets in config |
| **Disclosure policy** | Four tiers: `public` (verbatim), `internal` (summary), `confidential` (redacted), `secret` (never) | No formal disclosure policy |
| **Policy engine** | Validates every proposal against capabilities + ACLs | Simpler allowlist-based approval |

## 8. Deployment

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Primary target** | Gateway daemon (JSON-RPC + HTTP), CLI | Interactive CLI + messaging gateway |
| **Cloud/serverless** | HTTP API for remote agents | Modal, Daytona serverless backends |
| **Messaging platforms** | **Not implemented** — CLI + HTTP JSON-RPC only | Telegram, Discord, Slack, WhatsApp, Signal, Home Assistant, Email, SMS, Matrix, DingTalk, Feishu, WeCom |
| **Scheduling** | **Fully implemented**: background scheduler with 9 submodules — tick loop, wake predicates (timer, new_messages, task_completions, queued_work, approval_resolved), workflow task execution, approval gating, signal delivery | Built-in cron scheduler with natural language, duration/cron-expression schedules |
| **LLM providers** | 30+ providers via driver abstraction (OpenAI, Anthropic, Gemini, OpenRouter) | OpenRouter (200+ models), OpenAI, Anthropic, GLM, Kimi, MiniMax |

## 9. Research & Training

| Feature | Autonoetic | Hermes-Agent |
|---|---|---|
| **Trajectory capture** | Causal chain JSONL with full evidence mode | Batch trajectory generation, trajectory compression |
| **RL integration** | Not a focus | Atropos RL environments, Tinker-Atropos integration |
| **Training readiness** | N/A | Designed for training next-gen tool-calling models |

## 10. Feature Implementation Status

This table reflects what is actually implemented in the codebase, not just documented.

| Feature | Autonoetic Status | Hermes Status |
|---|---|---|
| **Learning tools** (`execution.search`, `knowledge.search_by_tags`, `digest.query`) | **Fully implemented** — native tools at `tools.rs`, integration tested | N/A (uses FTS5 session search instead) |
| **Post-session memory extraction** | **Fully implemented** — `post_session_digest.rs` (397 lines), wired into execution lifecycle | **Implemented** — background review thread |
| **Background scheduling** | **Fully implemented** — 9-module scheduler (`scheduler/`), tick loop, wake predicates, workflow tasks, approval gating, signal delivery | **Implemented** — cron scheduler with JSON job storage |
| **Context compression** | **Partial** — token counting, context %, session pruning, text truncation exist; no LLM summarization when approaching limits | **Implemented** — iterative summarization at configurable threshold |
| **Model fallback** | **Partial** — `fallback_provider`/`fallback_model` fields exist in types and checkpoints; no automatic runtime failover at driver level | **Implemented** — automatic fallback chain on rate limits/failures |
| **Smart model routing** | **Not implemented** — single provider/model per agent; `llm_preset_mapping` config field declared but never read | **Implemented** — cheap vs strong model based on task complexity |
| **User modeling** | **Not implemented** — no Honcho integration, no user profile system | **Implemented** — Honcho dialectic user modeling + `USER.md` |
| **Skill self-improvement** | **Partial** — evolution agents exist as static bundles; `agent.install` works; no autonomous closed-loop improvement | **Implemented** — skills auto-patch during use, autonomous creation after complex tasks |
| **Session search (FTS)** | **Not implemented** — `execution.search` covers tool traces only, not conversation content | **Implemented** — FTS5 across all session transcripts with LLM summarization |
| **Multi-channel adapters** | **Not implemented** — CLI + HTTP JSON-RPC only | **Implemented** — 15+ messaging platforms |

## 11. Maturity & Status

| Aspect | Autonoetic | Hermes-Agent |
|---|---|---|
| **Phase** | Runtime stabilization; learning infrastructure complete; automation loop and user-facing polish in progress | Production-ready (v0.6.0+), 3000+ tests |
| **Language** | Rust (high performance, memory safety) | Python (rapid development, large ecosystem) |
| **Ecosystem** | OFP federation, Python/TypeScript SDKs | agentskills.io standard, OpenClaw migration path |

---

## Key Insights

### What Autonoetic Has That Hermes Doesn't

1. **Strict separation of powers** — agents propose, gateway enforces. Hermes tools run directly in agent context.
2. **Capability-based security** — typed capabilities with pattern scoping vs allowlist-based approval.
3. **Sandboxing** — bubblewrap/docker/microvm vs terminal backends.
4. **Causal chain** — hash-chained immutable audit trail vs plain JSONL transcripts.
5. **Content-addressed artifacts** — SHA-256 store vs file-based.
6. **Cognitive Capsules** — portable bundle + runtime closure vs no formal export.
7. **Script mode** — deterministic fast path without LLM vs no equivalent.
8. **I/O schemas** — JSON Schema contracts on agents vs config-driven, no schema contracts.
9. **Disclosure policy** — four-tier output filtering vs no formal policy.
10. **Approval suspension** — turn continuation with disk persistence vs interactive approval only.
11. **Federation protocol** — OFP with HMAC handshake for multi-node vs multi-platform gateway only.

### What Hermes Has That Autonoetic Doesn't (Yet)

1. **Closed learning loop** — autonomous skill creation after complex tasks, self-improvement during use. Autonoetic has the primitives (learning tools, memory extraction, background scheduler) but hasn't wired the automation together.
2. **User modeling** — Honcho dialectic user modeling across sessions. Autonoetic has no user profiling.
3. **Messaging platforms** — 15+ platform adapters. Autonoetic has CLI + HTTP only.
4. **Context compression** — iterative summarization when approaching limits. Autonoetic tracks usage but doesn't compress.
5. **LLM provider failover** — automatic fallback chain. Autonoetic declares fallbacks but doesn't execute them.
6. **Smart model routing** — cheap vs strong model by task complexity. Autonoetic pins one model per agent.
7. **FTS session search** — full-text search across conversation content. Autonoetic searches execution traces only.
8. **Rich tool ecosystem** — 40+ built-in tools (browser, vision, TTS, etc.). Autonoetic has a smaller core set, relies on MCP for extension.
9. **Developer experience** — `curl | bash` → chat in 2 minutes vs Rust build, gateway setup, agent bundle creation.

### What Autonoetic Got Right (and Hermes Should Consider)

1. **Gateway mediation** — tool dispatch has a natural enforcement point for capability checking.
2. **Toolsets as convention** — YAML in SKILL.md, zero gateway code. Hermes' `toolsets.py` (542 lines) is overkill.
3. **Rust performance** — compile-time guarantees, memory safety.
4. **Immutable audit trail** — hash-chained causal chain for non-repudiable security.

### What Hermes Got Right (and Autonoetic Should Adopt)

1. **Closed learning loop automation** — the trigger → create → improve cycle should be automatic, not manual. Autonoetic's primitives are all there; they just need to be wired together.
2. **User modeling** — cross-session personalization is a real user need. Autonoetic should at minimum support a `USER.md` convention.
3. **Context compression** — iterative summarization prevents context window exhaustion on long sessions.
4. **Provider failover** — automatic fallback prevents silent failures during rate limits.
5. **FTS session search** — full-text search across conversation content is more useful than execution-trace-only search.

---

## Summary

**Autonoetic** is a **formal, governance-first runtime** built in Rust that enforces strict separation between reasoning and execution. It is designed for scenarios where auditability, reproducibility, and portable agent bundles (Cognitive Capsules) are paramount — an "operating system for agents" with capability-based security. Its learning infrastructure (search tools, memory extraction, background scheduler) is fully implemented; what's missing is the closed-loop automation and user-facing polish.

**Hermes-Agent** is a **practical, feature-rich AI assistant** built in Python with a built-in learning loop. It prioritizes daily utility — messaging integrations, 40+ tools, autonomous skill creation, cron scheduling, and RL training readiness — a "personal AI agent that gets better over time."

They solve overlapping but distinct problems: Autonoetic builds the **governed infrastructure** agents run on; Hermes-Agent builds the **agent itself** that users interact with. The ideal path forward for Autonoetic is to adopt Hermes' learning loop automation and user-facing features while keeping its principled architecture intact.

---

## Design Proposals: Closing the Gap

Hermes is single-user, single-node, single-LLM. Autonoetic must support multiple users, external approval entities (human or AI), distributed agents across nodes, and heterogeneous LLM providers. The following designs account for these constraints.

### 1. User Modeling

**Hermes approach:** One user → one `USER.md` + one Honcho profile. Simple, but doesn't scale.

**Autonoetic constraints:**
- Multiple users per gateway, each with separate identities
- Agents shared across users with different trust levels
- Cross-node federation (user A on node EU, user B on node US)
- Agents need bounded user context — not everything about a user should be visible to every agent
- External approval entities (human or AI) may grant/revoke user-agent bindings

**Design — three layers:**

| Layer | What | Where | Who controls |
|---|---|---|---|
| **User profile** | Facts about the user (preferences, context, constraints) | `user_profiles` table in `gateway.db`, keyed by `user_id` | User (via approval) or agent with `UserProfileWrite` capability |
| **Agent-user binding** | What each agent knows about each user | Scoped `knowledge.*` entries with `user:<id>` + `agent:<id>` tags | Agent, bounded by capability scope |
| **Cross-node profile** | Portable user model for federation | Signed profile export in cognitive capsule | User, explicit consent required |

**Schema:**
```sql
CREATE TABLE user_profiles (
    user_id TEXT PRIMARY KEY,
    display_name TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    trust_domain TEXT NOT NULL,
    origin_node_id TEXT NOT NULL,
    profile_json TEXT NOT NULL,
    profile_version INTEGER NOT NULL
);

CREATE TABLE user_agent_bindings (
    user_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    scope TEXT NOT NULL,           -- "full", "restricted", "task_only"
    granted_at TEXT NOT NULL,
    granted_by TEXT NOT NULL,      -- user or approval record id
    PRIMARY KEY (user_id, agent_id)
);
```

**Tool surface:**
```
user.profile.read(user_id?)        → profile (defaults to caller's bound user)
user.profile.update(fields)        → requires user approval or pre-granted scope
user.profile.share(agent_id, scope) → grants agent access
user.profile.revoke(agent_id)      → removes binding
```

**Wake injection:** At session start, the gateway injects a **bounded user context snippet** into the system prompt, computed from `user_agent_bindings.scope`:
- `full` → full profile
- `restricted` → preferences + constraints only
- `task_only` → nothing injected; agent must explicitly request via `user.profile.read`

**Federation:** Profile export is signed, carries `trust_domain`, imported as `foreign` until attested. Cross-node profile sharing requires explicit user consent recorded in the approval queue.

**Complexity:** ~500 lines Rust. Low risk — follows the same scoped data store + ACL pattern as `knowledge.*`.

---

### 2. Context Compression

**Hermes approach:** Linear iterative summarization of old messages when approaching context limit. Works for one conversation, one user.

**Autonoetic constraints:**
- Multi-turn sessions with tool chains and approval suspension points
- Turn continuation persists state to disk — context must be restorable exactly
- Multi-agent child sessions, each with own context window
- Distributed session fragments (future federation)
- Checkpoint resume must restore exact context state
- Different compression needs for active vs suspended sessions

**Design — three strategies, chosen by context:**

| Strategy | When | How | Who executes |
|---|---|---|---|
| **Summarize** | Approaching context limit during active session | LLM summarizes old turns into structured summary | Cheap model (configurable), no approval needed |
| **Archive** | Session suspended for approval | Full context preserved on disk; only summary in memory | Gateway (deterministic, no LLM) |
| **Prune** | Child agent context handoff | Parent summarizes what child needs, discards rest | Parent agent or gateway |

**Context state becomes explicit:**
```rust
pub struct ContextState {
    messages: Vec<Message>,
    summary: Option<String>,
    archived_turns: u64,
    checkpoint_handle: Option<String>,
    token_budget: u64,
    compression_threshold: f64,
}
```

**Summarize flow:**
1. Identify compressible range: all messages except last N (protected window)
2. Build summary request with cheap model (`compression_llm_preset` in config)
3. LLM returns structured summary: `{goals, decisions, open_items, key_facts}`
4. Replace compressed range with single `system` message containing summary
5. Write full compressed context to content store (for audit/restore)
6. Update `ContextState`

**Approval suspension is the easy case:** turn continuation already persists full state to disk. On resume, reload from checkpoint — no compression needed. The compressed summary is only for the active in-memory window.

**Distributed session fragments (future):** each node compresses its own fragment independently. Root session carries a `fragment_summaries` map: `{node_id: summary}`. No cross-node context sharing — only summaries travel.

**Complexity:** ~600 lines Rust. Medium risk — context manipulation is sensitive; bad compression breaks agent reasoning. Needs extensive testing with real sessions.

---

### 3. Smart Model Routing

**Hermes approach:** Analyze task complexity with LLM, route to cheap or strong model. Works for one user, one node, one set of providers.

**Autonoetic constraints:**
- Multiple LLM providers per agent (primary + fallback already in config, unused)
- Budget constraints per session (dollar and potentially energy)
- Approval gates for expensive operations (model switches)
- Distributed nodes with different LLM availability
- Script agents that need no LLM at all
- External approval entities may gate model upgrades
- Different routing strategies may be appropriate for different deployments

#### Design principle: pluggable strategy, not hard-wired heuristics

The gateway should not commit to one routing approach. Different deployments need different trade-offs:

- A cost-sensitive deployment wants deterministic budget-aware routing with no LLM overhead.
- A quality-sensitive deployment wants an LLM to evaluate task complexity before choosing.
- A pragmatic deployment wants heuristics first, LLM classification only for ambiguous cases.

The architecture separates three concerns:

1. **Routing context** — the multi-dimensional input signal (budget, complexity, time, energy)
2. **Model catalog** — what's available and what each option costs
3. **Router strategy** — pluggable logic that maps context + catalog → model selection

```
RoutingContext  ─┐
                 ├──▶  ModelRouter (trait)  ──▶  SelectedModel
ModelCatalog   ──┘
```

#### Routing context: the multi-dimensional input

Each routing decision receives a `RoutingContext` that captures the current state across four dimensions:

```rust
/// Immutable snapshot of routing-relevant state at decision time.
#[derive(Debug, Clone)]
pub struct RoutingContext {
    // --- Budget dimension ---
    pub budget: BudgetState,

    // --- Complexity dimension ---
    pub complexity: ComplexitySignals,

    // --- Time dimension ---
    pub time: TimeSignals,

    // --- Agent identity (for per-agent policy overrides) ---
    pub agent_id: String,
    pub session_id: String,
    pub turn_number: u32,
}

#[derive(Debug, Clone)]
pub struct BudgetState {
    /// Remaining dollar budget for this session (None = unlimited).
    pub remaining_usd: Option<f64>,
    /// Fraction of session budget consumed so far (0.0–1.0).
    pub consumed_fraction: Option<f64>,
    /// Remaining energy budget in kWh (None = not tracked).
    pub remaining_energy_kwh: Option<f64>,
    /// Per-agent monthly token budget remaining (from ResourceLimits).
    pub remaining_monthly_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ComplexitySignals {
    /// Number of tools available to the agent.
    pub tool_count: usize,
    /// Number of tool calls in the current turn so far.
    pub tool_calls_this_turn: usize,
    /// Whether the current message or goal involves code generation.
    pub involves_code: bool,
    /// Delegation depth (0 = root session).
    pub delegation_depth: u32,
    /// Number of prior failed turns in this session (retry indicator).
    pub prior_failures: u32,
    /// Approximate input token count for this request.
    pub input_tokens_estimate: u64,
    /// Whether the current task involves multi-step planning.
    pub multi_step: bool,
}

#[derive(Debug, Clone)]
pub struct TimeSignals {
    /// Whether a user is actively waiting for a response.
    pub interactive: bool,
    /// Optional deadline (e.g., workflow task timeout).
    pub deadline: Option<std::time::Instant>,
    /// Whether this is a background task (eval run, scheduled tick).
    pub background: bool,
    /// Elapsed wall-clock time in this session so far.
    pub session_elapsed: std::time::Duration,
}
```

The gateway assembles the `RoutingContext` from state it already tracks — token usage from `LlmExchangeUsage` (which already has `estimated_cost_usd`), tool availability from the manifest, session metadata from `SessionAgentBinding`, and turn counters from `AgentExecutor`. No new data collection infrastructure needed.

#### Model catalog: what's available

The catalog describes each available model's properties. This is configuration, not runtime inference.

```rust
/// A model option known to the gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub preset_name: String,          // "fast", "default", "strong"
    pub provider: String,
    pub model: String,
    pub capability_tier: CapabilityTier,
    pub cost: ModelCost,
    pub latency: ModelLatency,
    /// Whether this model is currently available (provider reachable, not rate-limited).
    pub available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CapabilityTier {
    Fast,       // Simple Q&A, summarization, formatting
    Standard,   // Code generation, analysis, multi-step reasoning
    Strong,     // Complex planning, architecture, novel problem-solving
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub input_per_mtok_usd: f64,      // $/million input tokens
    pub output_per_mtok_usd: f64,     // $/million output tokens
    pub energy_per_mtok_kwh: Option<f64>,  // kWh/million tokens (when provider reports it)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelLatency {
    pub median_ttft_ms: u64,          // time to first token (median)
    pub median_tps: u64,              // tokens per second (median)
}
```

Config-driven, loaded at startup from `gateway.yaml`:

```yaml
llm_presets:
  fast:
    provider: "openai"
    model: "gpt-4o-mini"
    capability_tier: "fast"
    cost:
      input_per_mtok_usd: 0.15
      output_per_mtok_usd: 0.60
    latency:
      median_ttft_ms: 200
      median_tps: 120
  default:
    provider: "anthropic"
    model: "claude-sonnet-4-20250514"
    capability_tier: "standard"
    cost:
      input_per_mtok_usd: 3.0
      output_per_mtok_usd: 15.0
    latency:
      median_ttft_ms: 400
      median_tps: 80
  strong:
    provider: "anthropic"
    model: "claude-opus-4-20250514"
    capability_tier: "strong"
    cost:
      input_per_mtok_usd: 15.0
      output_per_mtok_usd: 75.0
    latency:
      median_ttft_ms: 800
      median_tps: 40
```

#### The `ModelRouter` trait: pluggable strategy

The core abstraction is a trait that the gateway calls at each LLM invocation:

```rust
/// Pluggable model routing strategy.
///
/// The gateway calls `select()` before each LLM completion. Implementations
/// receive the full routing context and model catalog, and return a model
/// selection with an auditable rationale.
#[async_trait::async_trait]
pub trait ModelRouter: Send + Sync {
    /// Choose a model given the current context and available options.
    async fn select(
        &self,
        context: &RoutingContext,
        catalog: &[ModelEntry],
    ) -> RoutingDecision;

    /// Human-readable name for causal chain logging.
    fn strategy_name(&self) -> &str;
}

/// The output of a routing decision — always logged to the causal chain.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingDecision {
    pub selected_preset: String,
    pub provider: String,
    pub model: String,
    pub rationale: String,           // human-readable explanation
    pub signals_used: Vec<String>,   // which context dimensions influenced the decision
    pub fallback_chain: Vec<String>, // ordered presets to try if selected fails
}
```

The trait is `async` so LLM-based strategies can call a cheap model without blocking. Deterministic strategies return immediately.

#### Three built-in strategies

**Strategy 1: `DeterministicRouter`** — no LLM call, pure function of context signals.

```rust
pub struct DeterministicRouter {
    config: DeterministicRoutingConfig,
}

impl DeterministicRouter {
    async fn select(&self, ctx: &RoutingContext, catalog: &[ModelEntry]) -> RoutingDecision {
        // 1. Filter: remove models that would exceed remaining budget
        let feasible: Vec<_> = catalog.iter()
            .filter(|m| m.available)
            .filter(|m| self.within_budget(m, ctx))
            .collect();

        // 2. Determine minimum capability tier from complexity signals
        let min_tier = self.required_tier(ctx);

        // 3. Among feasible models meeting the tier floor,
        //    prefer lower latency if interactive, lower cost if background
        let scored: Vec<_> = feasible.iter()
            .filter(|m| m.capability_tier >= min_tier)
            .map(|m| (m, self.score(m, ctx)))
            .collect();

        // 4. Select highest-scoring, build fallback chain from remaining
        let best = scored.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        // ...
    }

    fn required_tier(&self, ctx: &RoutingContext) -> CapabilityTier {
        if ctx.complexity.delegation_depth > 1
            || ctx.complexity.multi_step
            || ctx.complexity.prior_failures > 2 {
            CapabilityTier::Strong
        } else if ctx.complexity.involves_code
            || ctx.complexity.tool_count > 4 {
            CapabilityTier::Standard
        } else {
            CapabilityTier::Fast
        }
    }

    fn score(&self, model: &ModelEntry, ctx: &RoutingContext) -> f64 {
        let cost_weight = if ctx.time.background { 0.7 } else { 0.3 };
        let latency_weight = if ctx.time.interactive { 0.7 } else { 0.3 };

        let cost_score = 1.0 / (1.0 + model.cost.input_per_mtok_usd);
        let latency_score = 1.0 / (1.0 + model.latency.median_ttft_ms as f64 / 1000.0);

        cost_score * cost_weight + latency_score * latency_weight
    }
}
```

Pros: zero overhead, fully auditable, no circular token spend.
Cons: fixed heuristics may misjudge novel task types.

**Strategy 2: `LlmClassifierRouter`** — asks a cheap model to assess task complexity.

```rust
pub struct LlmClassifierRouter {
    classifier_driver: Arc<dyn LlmDriver>,  // always a fast/cheap model
    classifier_model: String,
    deterministic_fallback: DeterministicRouter,  // used when classifier fails or times out
}

impl LlmClassifierRouter {
    async fn select(&self, ctx: &RoutingContext, catalog: &[ModelEntry]) -> RoutingDecision {
        // 1. Budget guard: if classifier call itself would exceed budget, fall back
        if self.classifier_too_expensive(ctx) {
            return self.deterministic_fallback.select(ctx, catalog).await;
        }

        // 2. Build a short classification prompt from context signals
        let prompt = self.build_classification_prompt(ctx);

        // 3. Call the cheap classifier with a tight timeout
        let classification = tokio::time::timeout(
            Duration::from_millis(2000),
            self.classify(&prompt),
        ).await;

        match classification {
            Ok(Ok(tier)) => {
                // Use the LLM-determined tier, but still respect budget/time constraints
                self.select_with_tier(tier, ctx, catalog)
            }
            _ => {
                // Timeout or error: deterministic fallback
                self.deterministic_fallback.select(ctx, catalog).await
            }
        }
    }

    fn build_classification_prompt(&self, ctx: &RoutingContext) -> String {
        // Structured prompt: "Given these signals, classify as fast/standard/strong"
        // Includes: tool count, code involvement, delegation depth, prior failures
        // Does NOT include the actual user message (privacy + token savings)
        format!(
            "Classify task complexity as fast, standard, or strong.\n\
             Tools available: {}\n\
             Involves code: {}\n\
             Delegation depth: {}\n\
             Prior failures this session: {}\n\
             Multi-step planning: {}\n\
             Reply with one word: fast, standard, or strong.",
            ctx.complexity.tool_count,
            ctx.complexity.involves_code,
            ctx.complexity.delegation_depth,
            ctx.complexity.prior_failures,
            ctx.complexity.multi_step,
        )
    }
}
```

Pros: adapts to novel task types better than fixed heuristics.
Cons: adds latency (1 cheap LLM call), costs tokens, classifier model must be reliable.

**Strategy 3: `HybridRouter`** — deterministic first, LLM only when ambiguous.

```rust
pub struct HybridRouter {
    deterministic: DeterministicRouter,
    llm_classifier: LlmClassifierRouter,
    ambiguity_threshold: f64,  // 0.0–1.0 confidence threshold
}

impl HybridRouter {
    async fn select(&self, ctx: &RoutingContext, catalog: &[ModelEntry]) -> RoutingDecision {
        // 1. Run deterministic analysis first (free)
        let det_decision = self.deterministic.select(ctx, catalog).await;
        let confidence = self.deterministic.confidence(ctx);

        // 2. If deterministic is confident, use it
        if confidence >= self.ambiguity_threshold {
            return det_decision;
        }

        // 3. Ambiguous case: consult LLM classifier
        //    (e.g., moderate tool count + code + first turn = unclear)
        let llm_decision = self.llm_classifier.select(ctx, catalog).await;

        // 4. Log both decisions for observability
        RoutingDecision {
            rationale: format!(
                "Hybrid: deterministic suggested '{}' (confidence {:.0}%), \
                 LLM classifier chose '{}'",
                det_decision.selected_preset,
                confidence * 100.0,
                llm_decision.selected_preset,
            ),
            ..llm_decision
        }
    }
}
```

Pros: best of both — fast in clear cases, adaptive in ambiguous ones. Most LLM calls are avoided.
Cons: slightly more complex to reason about; ambiguity threshold needs tuning.

#### Strategy selection is configuration, not code

The gateway config declares which strategy to use. Changing strategy requires no code changes.

```yaml
llm_routing:
  strategy: "deterministic"  # or "llm_classifier" or "hybrid"

  # Deterministic strategy config
  deterministic:
    budget_pressure_threshold: 0.7    # fraction consumed before downgrading
    energy_pressure_threshold: 0.8
    interactive_latency_bias: 0.7     # weight toward fast models for interactive sessions
    background_cost_bias: 0.7         # weight toward cheap models for background tasks

  # LLM classifier config (used by "llm_classifier" and "hybrid" strategies)
  llm_classifier:
    classifier_preset: "fast"         # always use the cheapest model for classification
    timeout_ms: 2000
    max_classifier_cost_usd: 0.001   # skip classifier if it would cost more than this

  # Hybrid config
  hybrid:
    ambiguity_threshold: 0.75         # confidence below this triggers LLM classification

  # Per-agent overrides (optional)
  agent_overrides:
    planner.default:
      min_capability_tier: "strong"   # planner always gets a strong model
    coder.default:
      min_capability_tier: "standard"

  # Approval gates
  approval_required:
    - strong_model_first_use          # first time a session uses a strong model
    - budget_threshold_crossed        # crossing 80% of session budget
```

#### Budget and energy tracking

Budget tracking extends the existing `LlmExchangeUsage` (which already has `estimated_cost_usd` in `autonoetic-types/src/agent.rs:65`) with session-level accumulation:

```rust
/// Tracks cumulative resource consumption for a session.
#[derive(Debug, Clone, Default)]
pub struct SessionBudget {
    pub total_cost_usd: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_energy_kwh: f64,
    pub limit_usd: Option<f64>,       // from session or agent config
    pub limit_energy_kwh: Option<f64>,
    pub routing_decisions: u32,       // number of model selections made
}

impl SessionBudget {
    /// Update after each LLM completion.
    pub fn record(&mut self, usage: &LlmExchangeUsage, model: &ModelEntry) {
        self.total_input_tokens += usage.input_tokens;
        self.total_output_tokens += usage.output_tokens;
        if let Some(cost) = usage.estimated_cost_usd {
            self.total_cost_usd += cost;
        }
        if let Some(energy_rate) = model.cost.energy_per_mtok_kwh {
            let total_tokens = usage.input_tokens + usage.output_tokens;
            self.total_energy_kwh += (total_tokens as f64 / 1_000_000.0) * energy_rate;
        }
    }

    pub fn consumed_fraction(&self) -> Option<f64> {
        self.limit_usd.map(|limit| (self.total_cost_usd / limit).min(1.0))
    }
}
```

Budget declarations can come from three sources (highest priority wins):

1. Session-level: `event.ingest` metadata carries `budget_usd` and `budget_energy_kwh`
2. Agent-level: SKILL.md manifest `limits.session_budget_usd`
3. Gateway-level: `gateway.yaml` default budget

#### Fallback chain (fixing existing dead code)

The fallback chain integrates with routing decisions. When the selected model fails (rate limit, provider down), the gateway tries the next model in `RoutingDecision.fallback_chain` without re-running the router:

```rust
async fn complete_with_routing(
    &self,
    request: &CompletionRequest,
    ctx: &RoutingContext,
) -> Result<(CompletionResponse, RoutingDecision)> {
    let decision = self.router.select(ctx, &self.catalog).await;

    // Log routing decision to causal chain
    self.tracer.log_routing_decision(&decision);

    // Try selected model first
    let driver = self.build_driver(&decision.provider, &decision.model)?;
    match driver.complete(request).await {
        Ok(response) => return Ok((response, decision)),
        Err(e) if is_retryable(&e) => {
            self.tracer.log_routing_fallback(&decision, &e);
        }
        Err(e) => return Err(e),
    }

    // Walk the fallback chain
    for preset in &decision.fallback_chain {
        if let Some(entry) = self.catalog.iter().find(|m| m.preset_name == *preset) {
            let driver = self.build_driver(&entry.provider, &entry.model)?;
            match driver.complete(request).await {
                Ok(response) => return Ok((response, decision)),
                Err(_) => continue,
            }
        }
    }

    Err(anyhow::anyhow!("No available provider in fallback chain"))
}
```

#### Causal chain integration

Every routing decision is logged to the causal chain, making the routing history fully auditable:

```rust
// In session_tracer.rs:
pub fn log_routing_decision(&mut self, decision: &RoutingDecision) {
    self.log_event(
        "routing",
        "model.select",
        EntryStatus::Success,
        Some(&decision.selected_preset),
        Some(&serde_json::to_string(decision).unwrap_or_default()),
    );
}
```

This means you can later query: "Why did session X use the strong model?" or "How much did routing overhead add to session cost?" — which neither Hermes nor any other framework currently supports.

#### Distributed LLM routing (future)

Peer nodes advertise their model catalog via `peer.describe()`. The local router's catalog merges local + peer entries. `execution.lease.request` carries the `RoutingDecision` as a constraint. Cross-node model calls use OFP transport to the remote node's driver. The `ModelRouter` trait is unchanged — it just sees a larger catalog.

#### Complexity breakdown

| Component | Lines (est.) | Risk |
|---|---|---|
| `RoutingContext` + `ModelCatalog` types | ~150 | Low — pure data types |
| `ModelRouter` trait + `RoutingDecision` | ~60 | Low — trait definition |
| `DeterministicRouter` | ~180 | Low — pure functions |
| `LlmClassifierRouter` | ~200 | Medium — cheap LLM call + timeout handling |
| `HybridRouter` | ~100 | Low — composition of above two |
| `SessionBudget` tracking | ~80 | Low — extends existing `LlmExchangeUsage` |
| Config parsing + strategy factory | ~120 | Low — follows existing config patterns |
| Causal chain logging | ~40 | Low — extends existing tracer |
| Fallback chain wiring | ~100 | Low-Medium — replaces dead `fallback_provider` code |
| Tests | ~200 | Low |
| **Total** | **~1230** | **Low-Medium overall** |

The increase from the original ~480 estimate reflects the pluggable architecture and multi-strategy support. However, an MVP could ship the `DeterministicRouter` alone (~600 lines) and add the LLM and hybrid strategies later — the trait boundary makes this safe.

---

### 4. FTS Session Search

**Hermes approach:** FTS5 on one SQLite DB for one user. Simple, effective, but doesn't handle multi-user ACLs or distributed nodes.

**Autonoetic constraints:**
- Multiple users with ACL-gated access to sessions
- Cross-agent sessions (agent A spawned agent B)
- Distributed session fragments (future)
- Causal chain (structured events) + session transcripts (conversation) are separate
- Execution traces already searchable via `execution.search` — this adds conversation content

#### Current state of conversation storage

Autonoetic already stores three kinds of session data — but none of it is searchable as conversation content:

| Data | Where stored | Searchable? | What it covers |
|---|---|---|---|
| **Conversation history** | Content store (SHA-256 blobs) via `persist_history_to_content_store()` in `lifecycle.rs` | ❌ No — blob-addressed, requires exact `session_id` to retrieve | Full message array (user/assistant/tool turns), merged across runs, redacted, bounded to 400 messages |
| **Execution traces** | `execution_traces` table in `gateway.db` | ✅ Yes, via `execution.search` tool | Structured tool execution records: command, stdout, stderr, exit code, duration, error type |
| **Causal chain** | JSONL files (`causal_chain-*.jsonl`) + `causal_events` table in `gateway.db` | ❌ Not directly — table queryable by `session_id`/`agent_id` but no text search tool exposed | Hash-chained event entries: tool invocations, approvals, turn boundaries, status changes |

**Key gap: conversation history is only persisted at hibernate/suspend points, not at session end.** The `persist_history_to_content_store()` function (lifecycle.rs:2123–2206) is called at line 1149 inside the hibernation yield branch only. The `close_session()` method (lifecycle.rs:313) does _not_ persist history — it only writes reevaluation state and a session summary. Normal sessions that complete without hibernating may never have their conversation stored.

Even when stored, the content store is content-addressed (SHA-256 blobs). Retrieving a session's history requires `store.read_by_name(session_id, "session_history")` with the exact session ID. There is no FTS5 index, no SQL table with searchable conversation text, and no tool that queries across session histories.

#### Prerequisite fix: always persist conversation history

Before FTS can work, conversation history must be persisted unconditionally at session end. This is a ~10-line change in `lifecycle.rs`:

```rust
// In close_session(), add before tracer.log_session_end():
if let Some(gateway_dir) = self.gateway_dir.as_ref() {
    // Build a minimal history from the current session state.
    // For sessions that already hibernated, merged history exists;
    // for sessions that completed normally, this is the first persist.
    if let Err(e) = persist_history_to_content_store(
        &self.agent_dir,
        &session_id,
        &[], // empty slice — function merges with any existing persisted history
        gateway_dir,
        &mut tracer,
        &disclosure_state,
    ) {
        tracing::warn!("Failed to persist history at session close: {}", e);
    }
}
```

Note: `persist_history_to_content_store` already handles the merge-with-existing case (lifecycle.rs:2139–2155): it reads any previously persisted `"session_history"` from the content store and appends new messages. Passing an empty slice triggers a "seal" — persisting whatever was accumulated during hibernation points, or acting as a no-op if no history was captured at all. A more complete fix would thread the final `history` slice through `close_session()` so the complete conversation is always captured, even for sessions that never hibernated.

#### Design — two-layer storage: metadata in `gateway.db`, full transcript in content store

Rather than storing the entire transcript JSON in a SQL column (which can grow very large — 400 messages × multi-KB each), split storage into:

1. **Metadata + searchable excerpt** → `session_transcripts` table in `gateway.db`, FTS5-indexed
2. **Full transcript blob** → existing content store, referenced by `transcript_handle`

This keeps `gateway.db` manageable while enabling full-text search. The existing content store already handles SHA-256 blobs, deduplication, and session-scoped naming — no new infrastructure needed for the blob side.

```sql
-- Schema migration (follows existing ordered migration pattern in gateway_store.rs)
CREATE TABLE session_transcripts (
    session_id TEXT PRIMARY KEY,
    root_session_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,          -- from session_agent_bindings
    user_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    status TEXT NOT NULL DEFAULT 'active',    -- active, completed, suspended, failed
    turn_count INTEGER NOT NULL DEFAULT 0,
    transcript_handle TEXT,             -- SHA-256 handle in content store for full transcript
    excerpt TEXT NOT NULL DEFAULT '',   -- bounded plaintext extract for FTS (≤ 8KB)
    origin_node_id TEXT NOT NULL        -- federation provenance
);

CREATE INDEX idx_session_transcripts_agent ON session_transcripts(agent_id, started_at DESC);
CREATE INDEX idx_session_transcripts_root ON session_transcripts(root_session_id);
CREATE INDEX idx_session_transcripts_user ON session_transcripts(user_id, started_at DESC);

-- FTS5 content-sync table: only indexes excerpt, agent_id, and user_id
-- session_id is UNINDEXED (used for joining, not for text search)
CREATE VIRTUAL TABLE session_transcripts_fts USING fts5(
    excerpt,
    agent_id,
    user_id,
    session_id UNINDEXED,
    content='session_transcripts',
    content_rowid='rowid'
);

-- Triggers to keep FTS in sync (standard FTS5 content-sync pattern)
CREATE TRIGGER session_transcripts_ai AFTER INSERT ON session_transcripts BEGIN
    INSERT INTO session_transcripts_fts(rowid, excerpt, agent_id, user_id, session_id)
    VALUES (new.rowid, new.excerpt, new.agent_id, new.user_id, new.session_id);
END;

CREATE TRIGGER session_transcripts_ad AFTER DELETE ON session_transcripts BEGIN
    INSERT INTO session_transcripts_fts(session_transcripts_fts, rowid, excerpt, agent_id, user_id, session_id)
    VALUES ('delete', old.rowid, old.excerpt, old.agent_id, old.user_id, old.session_id);
END;

CREATE TRIGGER session_transcripts_au AFTER UPDATE ON session_transcripts BEGIN
    INSERT INTO session_transcripts_fts(session_transcripts_fts, rowid, excerpt, agent_id, user_id, session_id)
    VALUES ('delete', old.rowid, old.excerpt, old.agent_id, old.user_id, old.session_id);
    INSERT INTO session_transcripts_fts(rowid, excerpt, agent_id, user_id, session_id)
    VALUES (new.rowid, new.excerpt, new.agent_id, new.user_id, new.session_id);
END;
```

**Excerpt extraction** converts the message array into a bounded plaintext string for FTS indexing:

```rust
fn extract_searchable_excerpt(messages: &[Message], max_bytes: usize) -> String {
    let mut buf = String::with_capacity(max_bytes);
    for msg in messages {
        if matches!(msg.role, Role::System) { continue; }
        let prefix = match msg.role {
            Role::User => "U: ",
            Role::Assistant => "A: ",
            _ => "",
        };
        if buf.len() + prefix.len() + msg.content.len() + 1 > max_bytes {
            let remaining = max_bytes.saturating_sub(buf.len() + prefix.len() + 1);
            buf.push_str(prefix);
            buf.push_str(&msg.content[..remaining.min(msg.content.len())]);
            break;
        }
        buf.push_str(prefix);
        buf.push_str(&msg.content);
        buf.push('\n');
    }
    buf
}
```

#### Integration point: wiring into session lifecycle

The transcript capture hooks into the existing `persist_history_to_content_store()` flow (lifecycle.rs:2123). After writing the full transcript to the content store, the same function (or a companion) upserts the `session_transcripts` row:

```rust
// After: store.register_name(session_id, "session_history", &history_handle)?;
// Add:
if let Some(gs) = gateway_store {
    let excerpt = extract_searchable_excerpt(&merged_history, 8192);
    gs.upsert_session_transcript(&SessionTranscriptRecord {
        session_id: session_id.to_string(),
        root_session_id: root_session_id(session_id).to_string(),
        agent_id: agent_id.to_string(),
        revision_id: revision_id.to_string(),  // from session_agent_bindings
        user_id: user_id.map(String::from),
        started_at: started_at.to_string(),
        ended_at: None,  // set at close_session
        status: "active".to_string(),
        turn_count: merged_history.len() as i64,
        transcript_handle: Some(history_handle.clone()),
        excerpt,
        origin_node_id: origin_node_id.to_string(),
    })?;
}
```

This means the FTS index updates incrementally at every hibernate point and at session close. No batch job needed.

#### `session.search` tool

Follows the same native tool pattern as `execution.search` (tools.rs:2991–3125) — the implementation structure is nearly identical.

```
session.search(
    query: string,              -- FTS5 query (supports AND/OR/NOT/phrase)
    agent_id?: string,          -- filter by agent
    user_id?: string,           -- defaults to caller's bound user
    session_id?: string,        -- exact session or prefix match
    root_session_id?: string,   -- search across a workflow's sessions
    status?: string,            -- active, completed, suspended, failed
    since?: string,             -- RFC3339 cutoff
    limit?: number              -- default 10, max 100
) → {
    ok: true,
    results: [{
        session_id, root_session_id, agent_id, revision_id,
        user_id, started_at, ended_at, turn_count, status,
        excerpt,                -- matching text snippet from FTS5
        score,                  -- FTS5 rank score
        transcript_handle       -- content store handle for full transcript
    }],
    count: number
}
```

The query uses FTS5 `MATCH` with `bm25()` ranking:

```sql
SELECT t.session_id, t.root_session_id, t.agent_id, t.revision_id,
       t.user_id, t.started_at, t.ended_at, t.turn_count, t.status,
       snippet(session_transcripts_fts, 0, '[', ']', '...', 32) AS excerpt,
       bm25(session_transcripts_fts) AS score,
       t.transcript_handle
FROM session_transcripts t
JOIN session_transcripts_fts fts ON fts.session_id = t.session_id
WHERE session_transcripts_fts MATCH ?1
  AND (?2 IS NULL OR t.agent_id = ?2)
  AND (?3 IS NULL OR t.user_id = ?3)
  AND (?4 IS NULL OR t.started_at >= ?4)
  AND (?5 IS NULL OR t.status = ?5)
ORDER BY score
LIMIT ?6
```

#### ACL enforcement (query-time filter)

Follows the same capability-check pattern used by `execution.search` and `knowledge.search_by_tags`. The key constraint: agents should only see sessions they participated in, their child sessions, or sessions explicitly shared with them.

```rust
fn enforce_search_acl(
    caller_agent_id: &str,
    caller_session_id: Option<&str>,
    results: &[SessionSearchResult],
    store: &GatewayStore,
) -> Vec<SessionSearchResult> {
    results.iter().filter(|r| {
        // Agent can see its own sessions
        r.agent_id == caller_agent_id
            // Agent can see child sessions of its current root session
            || caller_session_id.map_or(false, |sid| {
                let root = root_session_id(sid);
                r.root_session_id == root
            })
            // Future: explicit sharing via user_agent_bindings
    }).cloned().collect()
}
```

#### `session.summarize` (optional, separate tool)

Post-processing step on search results. Uses a cheap model to summarize multiple sessions. Not part of FTS5.

```
session.summarize(session_ids: [string]) → {
    summary, common_themes, decisions, open_items
}
```

For each session ID, reads the full transcript from the content store via `transcript_handle`, then sends to the configured compression model. This reuses the same cheap-model infrastructure proposed in the context compression design.

#### Relationship to existing search tools

After this feature, autonoetic has three complementary search surfaces:

| Tool | What it searches | Data source | Use case |
|---|---|---|---|
| `execution.search` | Tool execution records (commands, stdout, errors) | `execution_traces` table | "What commands failed?" "How did we fix that build error?" |
| `knowledge.search_by_tags` | Durable facts stored by agents | Tier 2 memory (per-agent SQLite) | "What API patterns did we learn?" "What user preferences exist?" |
| `session.search` (**new**) | Conversation content across sessions | `session_transcripts` + FTS5 | "When did we discuss the caching strategy?" "Find sessions where we debugged timeouts" |

The post-session digest (`post_session_digest.rs`) bridges conversation → knowledge: it extracts structured memories from conversation content and stores them as Tier 2 knowledge. `session.search` provides the raw conversation search that complements those extracted memories.

#### Distributed search (future)

Each node has its own FTS5 index. `peer.search(query, constraints)` broadcasts to peers. Results merged with `origin_node_id` metadata. No cross-node FTS index — query fan-out + merge. The `origin_node_id` on `session_transcripts` already enables this.

#### Complexity breakdown

| Component | Lines (est.) | Risk |
|---|---|---|
| Schema migration (SQL + Rust migration entry) | ~80 | Low — follows existing `schema_migrations` pattern |
| `extract_searchable_excerpt()` + transcript persistence | ~120 | Low — extends existing `persist_history_to_content_store()` |
| `close_session()` fix (always persist history) | ~15 | Low — critical prerequisite |
| `GatewayStore` methods (upsert, search, ACL) | ~200 | Low — mirrors `search_execution_traces()` pattern |
| `SessionSearchTool` native tool | ~180 | Low — mirrors `ExecutionSearchTool` structure |
| `SessionSummarizeTool` (optional) | ~120 | Medium — requires LLM call integration |
| Tests | ~200 | Low |
| **Total** | **~915** | **Low overall** |

Main risk is not the FTS5 implementation (battle-tested SQLite) but ensuring the excerpt extraction and ACL filtering are correct. The bulk of the work is wiring into the existing session lifecycle, which is well-understood from the existing `persist_history_to_content_store()` and `execution_traces` patterns.

---

### Implementation Summary

| Feature | Lines (est.) | Complexity | Dependencies | Independent? |
|---|---|---|---|---|
| **User modeling** | ~500 | Medium | None | Yes |
| **Context compression** | ~600 | Medium-High | Checkpoint system (exists) | Yes |
| **Smart model routing** | ~1230 (~600 MVP) | Low-Medium | `LlmExchangeUsage.estimated_cost_usd`, `fallback_provider` config, `LlmDriver` trait | Yes (MVP ships `DeterministicRouter` only) |
| **FTS session search** | ~915 | Medium | Session lifecycle (exists), `persist_history_to_content_store()` fix | Yes (prerequisite fix is internal) |

All four are **independent** — no ordering dependency. Lowest-risk starting point: **smart model routing** (config fields already exist, mostly wiring). Highest-value: **FTS session search** (unlocks learning tools with actual conversation content, complementing existing `execution.search` and `knowledge.search_by_tags`).

The distributed/federation considerations don't fundamentally change any design — they add metadata fields (`origin_node_id`, `trust_domain`) and ACL checks consistent with existing Autonoetic patterns. The key difference from Hermes is that every feature must respect multi-user boundaries, approval gates, and cross-node provenance from the start.
