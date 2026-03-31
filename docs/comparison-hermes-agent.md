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
- Budget constraints per session
- Approval gates for expensive operations (model switches)
- Distributed nodes with different LLM availability
- Script agents that need no LLM at all
- External approval entities may gate model upgrades

**Design — policy-driven routing, not dynamic guessing:**

```
Task → Complexity estimate → Policy decision → Model selection
```

**Complexity classification (heuristic, no LLM needed):**
```rust
fn estimate_complexity(message: &str, tools: &[ToolRef]) -> ComplexityClass {
    match (msg_len, tool_count, has_code, delegation_depth) {
        (short, 0..=1, false, 0) => Simple,
        (medium, 2..=4, maybe, 0..=1) => Moderate,
        _ => Complex,
    }
}
```

**Three routing modes:**

| Mode | How it works | Who decides |
|---|---|---|
| **Manual** | Agent manifest declares `llm_config` per role | User/admin (current behavior) |
| **Policy-driven** | `llm_preset_mapping` maps complexity classes to presets | Gateway policy engine |
| **Budget-aware** | Auto-downgrades when session budget depletes | Gateway (deterministic) |

**Config (fields already exist, just need wiring):**
```yaml
llm_presets:
  fast:
    provider: "openai"
    model: "gpt-4o-mini"
  default:
    provider: "anthropic"
    model: "claude-sonnet-4-20250514"
  strong:
    provider: "anthropic"
    model: "claude-opus-4-20250514"

llm_preset_mapping:
  planner.default: "strong"
  coder.default: "default"
  researcher.default: "default"

llm_routing:
  enabled: true
  complexity_presets:
    simple: "fast"
    moderate: "default"
    complex: "strong"
  budget_downgrade:
    enabled: true
    threshold: 0.8
    downgrade_to: "fast"
  approval_required:
    - model_change
    - strong_model_use
```

**Fallback chain (fixing existing dead code):**
```rust
async fn complete_with_fallback(&self, request: &CompletionRequest) -> Result<CompletionResponse> {
    match self.primary.complete(request).await {
        Ok(response) => return Ok(response),
        Err(RateLimit | ProviderError) => {}
    }
    if let Some(fallback) = &self.config.fallback_provider {
        return self.build_driver(fallback).complete(request).await;
    }
    if self.budget.remaining_pct() < 0.5 {
        return self.complete_with_preset("fast", request).await;
    }
    Err(NoAvailableProvider)
}
```

**Distributed LLM routing (future):** peer nodes advertise available providers/models. `execution.lease.request` carries `required_llm: "strong"` constraint. Cross-node model call uses the remote node's driver.

**Complexity:** ~480 lines Rust. Low-moderate risk — the data model already exists, just needs wiring. Main risk is budget downgrade triggering unexpectedly.

---

### 4. FTS Session Search

**Hermes approach:** FTS5 on one SQLite DB for one user. Simple, effective, but doesn't handle multi-user ACLs or distributed nodes.

**Autonoetic constraints:**
- Multiple users with ACL-gated access to sessions
- Cross-agent sessions (agent A spawned agent B)
- Distributed session fragments (future)
- Causal chain (structured events) + session transcripts (conversation) are separate
- Execution traces already searchable via `execution.search` — this adds conversation content

**Design — FTS5 on `gateway.db` with ACL enforcement at query time:**

```sql
CREATE TABLE session_transcripts (
    session_id TEXT PRIMARY KEY,
    root_session_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    agent_revision TEXT NOT NULL,
    user_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    status TEXT NOT NULL,
    turn_count INTEGER NOT NULL,
    transcript_json TEXT
);

CREATE VIRTUAL TABLE session_transcripts_fts USING fts5(
    transcript_json,
    agent_id,
    user_id,
    session_id UNINDEXED,
    content='session_transcripts'
);
```

**Tool:**
```
session.search(
    query: string,
    agent_id?: string,
    user_id?: string,        -- defaults to caller's user
    session_id?: string,
    status?: string,
    since?: string,
    limit?: number
) → [{ session_id, agent_id, agent_rev, user_id, started_at,
       turn_count, status, excerpt, score, transcript_handle }]
```

**ACL enforcement (query-time filter):**
```rust
fn enforce_search_acl(caller: &AgentIdentity, results: &[SessionSearchResult], store: &GatewayStore)
    -> Vec<SessionSearchResult>
{
    results.iter().filter(|r| {
        r.user_id == caller.user_id
            || store.is_child_session(r.session_id, caller.session_id)
            || store.is_shared_with(r.session_id, caller.agent_id)
    }).collect()
}
```

**LLM summarization (optional, separate tool):**
```
session.summarize(session_ids: [string]) → {
    summary, common_themes, decisions, open_items
}
```
Uses a cheap model to summarize multiple session transcripts. Post-processing step on search results, not part of FTS5.

**Distributed search (future):** each node has its own FTS5 index. `peer.search(query, constraints)` broadcasts to peers. Results merged with node origin metadata. No cross-node FTS index — query fan-out + merge.

**Complexity:** ~680 lines Rust + 80 lines SQL. Low risk — FTS5 is battle-tested SQLite. Main work is wiring transcript capture into existing session lifecycle (which already saves to causal chain and content store).

---

### Implementation Summary

| Feature | Lines (est.) | Complexity | Dependencies | Independent? |
|---|---|---|---|---|
| **User modeling** | ~500 | Medium | None | Yes |
| **Context compression** | ~600 | Medium-High | Checkpoint system (exists) | Yes |
| **Smart model routing** | ~480 | Low-Medium | `llm_preset_mapping` config (exists, unused) | Yes |
| **FTS session search** | ~680 | Medium | Session lifecycle (exists) | Yes |

All four are **independent** — no ordering dependency. Lowest-risk starting point: **smart model routing** (config fields already exist, mostly wiring). Highest-value: **FTS session search** (unlocks learning tools with actual conversation content, complementing existing `execution.search` and `knowledge.search_by_tags`).

The distributed/federation considerations don't fundamentally change any design — they add metadata fields (`origin_node_id`, `trust_domain`) and ACL checks consistent with existing Autonoetic patterns. The key difference from Hermes is that every feature must respect multi-user boundaries, approval gates, and cross-node provenance from the start.
