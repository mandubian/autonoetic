# Plan: Hermes Gap Closure

**Triggered by:** [comparison-hermes-agent.md](comparison-hermes-agent.md) — analysis of features Hermes has that Autonoetic lacks, with detailed designs for closing each gap.

**Goal:** Implement 7 independent features to close the capability gap with Hermes-Agent while preserving Autonoetic's governance-first architecture (multi-user, approval gates, separation of powers, federation-ready).

---

## Priority Order (recommended)

All 7 features are independent — no ordering dependency. Priority is based on immediate impact and risk.

| # | Feature | Est. Lines (MVP / Full) | Risk | Rationale |
|---|---------|-------------------------|------|-----------|
| 1 | Prompt budget transparency | 200 / 660 | Low | Immediate token savings + data to guide all other features |
| 2 | Smart model routing | 600 / 1,230 | Low-Med | Wires existing dead config fields, budget-aware routing |
| 3 | Credential management | 250 / 1,130 | Low-Med | Unblocks agent-to-service interaction |
| 4 | FTS session search | — / 915 | Medium | Biggest learning infrastructure unlock |
| 5 | Agent Skills compatibility | — / 820 | Low | Ecosystem growth enabler |
| 6 | User modeling | — / 500 | Medium | Lower urgency until multi-user is live |
| 7 | Context compression | — / 600 | Med-High | Highest risk, needs quality regression framework |

---

## Feature 1: Prompt Budget Transparency

**Problem:** System prompt exceeds 10K tokens before the first user turn. No visibility into what consumes the budget, no mechanism to control it.

**Key files:** `lifecycle.rs` (prompt assembly at line 584-612), `foundation_instructions.md`

**Status (2026-04-01):** Fully implemented through commit range covering Features 1A–1E + strategy refactoring.
- Completed: prompt budget breakdown, token estimation heuristic, pre-LLM logging, foundation layering (with layer-selection tests), prompt budget config parsing, strategy-pattern enforcement (Warn/TrimHistory/DemoteTools/Fail), tool schema compression gated by config (empirically validated with real LLMs), tool tiering with manifest-level filtering, per-section cap enforcement, tool-call group preservation during trimming, and unit coverage for all pieces.
- Still open: workflow-state-aware tier filtering (Phase E).
- **Documentation:** [prompt-budget.md](prompt-budget.md)

### Tasks

#### Phase A — MVP: Observability + Tool Tiering (~200 lines)

- [x] **1A.1** Create `PromptBudgetBreakdown` struct (system prompt, tool definitions, conversation history, total, utilization %)
- [x] **1A.2** Add `estimate_tokens()` heuristic function (~4 chars/token)
- [x] **1A.3** Insert pre-LLM-call breakdown computation in `lifecycle.rs` (after tools assembled, before `CompletionRequest`)
- [x] **1A.4** Log breakdown to causal chain + `tracing::info!`
- [x] **1A.5** Add `ToolTier` enum (`Core`, `Workflow`, `Specialized`) — tier derived from tool name prefix via central classifier; `NativeTool::tier()` provides override point
- [x] **1A.6** Filter tool definitions by tier at collection time based on agent manifest (`allowed_tool_tiers` field)
  - *Deferred: workflow-state-aware filtering (approval gates, active workflow phase)*
- [x] **1A.7** Tests for breakdown accuracy and tool tiering filter

#### Phase B — Progressive Foundation Instructions (~80 lines)

- [x] **1B.1** Split `foundation_instructions.md` into layers: `foundation_core.md`, `foundation_workflow.md`, `foundation_artifact.md`, `foundation_script.md`, `foundation_digest.md`
- [x] **1B.2** Add `compose_foundation()` that selects layers based on agent capabilities
- [x] **1B.3** Tests for layer selection (8 tests covering core, workflow, artifact, script, digest layers)

#### Phase C — Token Budget Enforcement (~150 lines)

- [x] **1C.1** Add `prompt_budget` config section in `gateway.yaml` (system_prompt_max_tokens, tool_definitions_max_tokens, warn_at_pct, margin_tokens)
- [x] **1C.2** Implement budget enforcement: warn, trim history (preserving tool-call groups), or demote tools (with verification) when exceeded — strategy pattern with `BudgetEnforcementStrategy` trait
- [x] **1C.3** Tests for enforcement behavior (8 tests covering all strategies + section-cap violations)

#### Phase D — Tool Definition Compression (~80 lines)

- [x] **1D.1** Implement turn-aware tool compression: full schemas on turn 0, minimal on subsequent turns (gated by `compress_tool_schemas_after_turn_0` config)
- [x] **1D.2** Empirical validation per model — `openrouter_integration::test_openrouter_tool_compression` validates compressed schemas still produce valid tool calls on turn > 0

#### Phase E — Workflow-State-Aware Tier Filtering (~60 lines)

- [ ] **1E.1** Approval gate restriction: when session has pending approvals, restrict to Core + Workflow tiers only
- [ ] **1E.2** Child agent handoff narrowing: child sessions get Core-only tools unless parent explicitly requests more
- [ ] **1E.3** Tests for workflow-state-aware filtering

---

## Feature 2: Smart Model Routing

**Problem:** Single provider/model per agent. `fallback_provider`/`fallback_model` fields exist in types but are dead code. No automatic failover or cost-aware routing.

**Key files:** `autonoetic-types/src/agent.rs` (LlmExchangeUsage.estimated_cost_usd), LlmDriver trait, checkpoint types

### Tasks

#### Phase A — MVP: Types + Deterministic Router (~600 lines)

- [ ] **2A.1** Define `RoutingContext` struct (BudgetState, ComplexitySignals, TimeSignals, agent_id, session_id, turn_number)
- [ ] **2A.2** Define `ModelEntry`, `CapabilityTier`, `ModelCost`, `ModelLatency` types
- [ ] **2A.3** Define `ModelRouter` trait + `RoutingDecision` struct (async, with strategy_name, rationale, fallback_chain)
- [ ] **2A.4** Implement `DeterministicRouter` (budget filter, tier floor from complexity signals, cost/latency scoring)
- [ ] **2A.5** Add `SessionBudget` tracking struct (extends existing `LlmExchangeUsage`)
- [ ] **2A.6** Parse `llm_presets` and `llm_routing` config sections from `gateway.yaml`
- [ ] **2A.7** Assemble `RoutingContext` from existing state in `AgentExecutor` (token usage, manifest, session metadata)
- [ ] **2A.8** Wire `complete_with_routing()` into the LLM call path with fallback chain
- [ ] **2A.9** Log routing decisions to causal chain
- [ ] **2A.10** Tests for deterministic routing, budget scoring, fallback chain

#### Phase B — LLM Classifier Router (~200 lines)

- [ ] **2B.1** Implement `LlmClassifierRouter` (cheap model classification prompt, 2s timeout, deterministic fallback)
- [ ] **2B.2** Budget guard: skip classifier when classifier cost exceeds threshold
- [ ] **2B.3** Tests

#### Phase C — Hybrid Router (~100 lines)

- [ ] **2C.1** Implement `HybridRouter` (deterministic first, LLM only when confidence < ambiguity_threshold)
- [ ] **2C.2** Tests

#### Phase D — Per-Agent Overrides + Approval Gates

- [ ] **2D.1** Support `agent_overrides` in config (min_capability_tier per agent)
- [ ] **2D.2** Approval gates for strong_model_first_use and budget_threshold_crossed
- [ ] **2D.3** Tests

---

## Feature 3: Credential Management

**Problem:** Agents can't interact with external services requiring registration/authentication without leaking secrets into the LLM context.

**Key files:** `vault.rs` (Vault, ~78 lines), `runtime/store.rs` (SecretStoreRuntime, ~183 lines), `runtime/tool_call_processor.rs` (pipeline integration)

**Existing infrastructure:** Vault (SecretString anti-leak), SecretStoreRuntime (JSON extraction + redaction), DisclosureState taint, ToolCallProcessor pipeline integration.

### Tasks

#### Phase A — MVP: Pre-configured Credentials (~250 lines)

- [x] **3A.1** Define `CredentialRecord` struct (credential_id, service, secret_name, inject_as, created_by_agent, expires_at, shared_with, allowed_hosts)
- [x] **3A.2** Add `credentials` table schema migration in `gateway_store.rs`
- [x] **3A.3** Implement `credential.check` tool (query CredentialRecord by service name)
- [x] **3A.4** Implement `credential.request` tool (fetch secret from Vault, inject into HTTP request via reqwest, sanitize response)
- [x] **3A.5** Add `CredentialAccess` capability type with service-scoped patterns
- [x] **3A.6** Tests for check/request flow with pre-configured credentials

**Remaining issues:**
- Test flakiness risk from process-wide `AUTONOETIC_VAULT_PATH` env mutation under parallel test execution (`credential_integration.rs:77`). 4 tests are `#[ignore]`d. Fix: switch to config-based vault path threading.

#### Phase B — Automated Registration (~350 lines)

- [x] **3B.1** Define `CredentialSetupStep` enum (ApiCall, UserPrompt, UserAction) in `autonoetic-types/src/agent.rs`
- [x] **3B.2** Implement `credential.setup` tool (multi-step server-side execution, extract_secrets via JSONPath, store in Vault)
- [x] **3B.3** Extend JSONPath parser to accept `$`-prefixed notation (`parse_json_path()` in `runtime/store.rs`)
- [x] **3B.4** Wire approval queue integration: UserPrompt step creates `ApprovalRequest` with `ScheduledAction::CredentialPrompt`, returns `approval_request_id`, and breaks iteration
- [x] **3B.5** Tests for automated registration flow (8 tests: availability, service/network denial, user_action, user_prompt suspension with approval_id, extract_public overlap blocking, JSONPath parsing)

**Design decisions:**
- Multi-secret setups: only the first extracted secret name is persisted in `CredentialRecord`. Additional secrets are stored in the vault but not tracked in the record. One credential = one `secret_name`.
- JSONPath overlap normalization: `trim_start_matches('$').trim_start_matches('.')` is sufficient because `extract_json_path()` only resolves a minimal subset (dot-separated object keys, no brackets/wildcards/filters). Edge cases like whitespace or consecutive dots (`$.data..token`) are not normalized — if stronger canonicalization is needed, compare `Vec<String>` segments instead of strings.

**Fixes applied post-review:**
1. (High) UserPrompt now suspends immediately — breaks step iteration, returns `ok:false suspended:true approval_required:true` with `approval_request_id`
2. (High) `extract_public` paths overlapping `extract_secrets` paths are silently dropped (prevents secret exfiltration); paths normalized by stripping `$`/`.` prefix
3. (Medium/High) Multi-secret setup: only first secret stored in CredentialRecord (by design)
4. (Medium) Approval queue integration: `ScheduledAction::CredentialPrompt` variant added; UserPrompt creates approval request in store
5. (Low) Empty host explicitly denied in network policy check

#### Phase C — Encryption at Rest (~100 lines)

- [ ] **3C.1** Add AES-256-GCM encryption to `Vault.persist_to_file()` / `load_from_file()`
- [ ] **3C.2** Master key sources: env var, passphrase at boot, OS keychain (optional)
- [ ] **3C.3** Tests for encrypt/decrypt round-trip

#### Phase D — Secure User Prompt Channel (~200 lines)

- [ ] **3D.1** Implement out-of-band secure prompt (TUI/CLI password prompt) for human-assisted credential entry
- [ ] **3D.2** Wire into `credential.setup` UserPrompt step type
- [ ] **3D.3** Tests

#### Phase E — Advanced Features

- [ ] **3E.1** Token refresh/rotation handling (transparent 401 retry with refresh token)
- [ ] **3E.2** Credential sharing between agents (`credential.share` + approval)
- [ ] **3E.3** Credential expiry tracking and re-setup flow

---

## Feature 4: FTS Session Search

**Problem:** `execution.search` covers tool traces only, not conversation content. Session history is only persisted at hibernate points, not at session end. No full-text search across conversations.

**Key files:** `lifecycle.rs` (persist_history_to_content_store at line 2123, close_session at line 313), `gateway_store.rs`

### Tasks

#### Phase A — Prerequisite: Always Persist Conversation History (~15 lines)

- [ ] **4A.1** Add `persist_history_to_content_store()` call in `close_session()` before `tracer.log_session_end()`
- [ ] **4A.2** Thread final `history` slice through `close_session()` for complete capture

#### Phase B — Schema + Storage (~200 lines)

- [ ] **4B.1** Add `session_transcripts` table schema migration (session_id, root_session_id, agent_id, revision_id, user_id, started_at, ended_at, status, turn_count, transcript_handle, excerpt, origin_node_id)
- [ ] **4B.2** Create FTS5 virtual table `session_transcripts_fts` with content-sync triggers
- [ ] **4B.3** Implement `extract_searchable_excerpt()` (messages to bounded plaintext, max 8KB)
- [ ] **4B.4** Add `GatewayStore.upsert_session_transcript()` method

#### Phase C — Lifecycle Integration (~120 lines)

- [ ] **4C.1** Wire transcript upsert into `persist_history_to_content_store()` flow (after content store write)
- [ ] **4C.2** Update `close_session()` to set `ended_at` and `status` on transcript record

#### Phase D — session.search Tool (~180 lines)

- [ ] **4D.1** Implement `SessionSearchTool` native tool (FTS5 MATCH with bm25 ranking, filters by agent_id, user_id, session_id, status, since)
- [ ] **4D.2** Implement `enforce_search_acl()` (agent sees own sessions + child sessions of current root)
- [ ] **4D.3** Register tool in tools module, add capability gating
- [ ] **4D.4** Tests for search, ACL filtering, ranking

#### Phase E — session.summarize Tool (optional, ~120 lines)

- [ ] **4E.1** Implement `SessionSummarizeTool` (reads full transcripts via transcript_handle, summarizes with cheap model)
- [ ] **4E.2** Tests

---

## Feature 5: Agent Skills (agentskills.io) Compatibility

**Problem:** External Agent Skills from the agentskills.io ecosystem should be importable into Autonoetic. Both use SKILL.md but with different frontmatter schemas and trust models.

**Key files:** Agent manifest parser, content store, agent install system

**Key insight:** YAML namespaces already coexist — Autonoetic nests under `metadata.autonoetic`, so an Autonoetic SKILL.md is already a valid Agent Skills file. Import mostly means adding the `metadata.autonoetic` block.

### Tasks

#### Phase A — Frontmatter Adapter (~200 lines)

- [ ] **5A.1** Define `AgentSkillsFrontmatter` parser (name, description, license, compatibility, allowed-tools)
- [ ] **5A.2** Implement `adapt_agentskills_frontmatter()` mapping to `AutonoeticManifest`
- [ ] **5A.3** Implement `infer_capabilities()` from `allowed-tools` (Bash patterns, Read/Write, WebSearch/WebFetch)

#### Phase B — Tool Name Bridging (~60 lines)

- [ ] **5B.1** Generate tool name mapping table (Bash -> sandbox.exec, Read -> content.read, etc.)
- [ ] **5B.2** Inject mapping as instruction appendix into imported agent's system prompt (~200 tokens)

#### Phase C — Resource Mounting (~100 lines)

- [ ] **5C.1** Import `scripts/`, `references/`, `assets/` directories into agent dir under `imported/`
- [ ] **5C.2** Register imported files as content store entries
- [ ] **5C.3** Configure sandbox mounts for expected paths

#### Phase D — Progressive Disclosure (~80 lines)

- [ ] **5D.1** Adopt 3-tier disclosure for all agents: metadata only at startup, full body when activated, resources on demand
- [ ] **5D.2** Wire into `compose_system_instructions` (cross-cuts with Feature 1 prompt optimization)

#### Phase E — Trust Modes + CLI (~230 lines)

- [ ] **5E.1** Implement trust mode selection: Generous (auto-grant), Strict (approval per capability), Audit (dry-run sandbox)
- [ ] **5E.2** Add `agent.import_skill` tool or CLI command
- [ ] **5E.3** Tests for import flow, capability inference, trust modes

---

## Feature 6: User Modeling

**Problem:** No user profiling or cross-session personalization. Hermes has Honcho dialectic user modeling + USER.md.

**Key files:** `gateway_store.rs`, `knowledge.*` tools

**Autonoetic constraints:** Multi-user, per-agent scoped visibility, cross-node federation, external approval entities.

### Tasks

#### Phase A — Schema + Storage (~150 lines)

- [ ] **6A.1** Add `user_profiles` table (user_id, display_name, trust_domain, origin_node_id, profile_json, profile_version)
- [ ] **6A.2** Add `user_agent_bindings` table (user_id, agent_id, scope: full/restricted/task_only, granted_at, granted_by)
- [ ] **6A.3** GatewayStore CRUD methods for both tables

#### Phase B — Tool Surface (~200 lines)

- [ ] **6B.1** Implement `user.profile.read` tool (defaults to caller's bound user, respects binding scope)
- [ ] **6B.2** Implement `user.profile.update` tool (requires user approval or pre-granted scope)
- [ ] **6B.3** Implement `user.profile.share` tool (grants agent access, creates binding)
- [ ] **6B.4** Implement `user.profile.revoke` tool (removes binding)
- [ ] **6B.5** Add `UserProfileWrite` capability type

#### Phase C — Wake Injection (~80 lines)

- [ ] **6C.1** At session start, inject bounded user context snippet into system prompt based on binding scope
- [ ] **6C.2** Scope rules: `full` = full profile, `restricted` = preferences + constraints only, `task_only` = nothing injected

#### Phase D — Federation (~70 lines)

- [ ] **6D.1** Profile export (signed, carries trust_domain)
- [ ] **6D.2** Import as `foreign` until attested
- [ ] **6D.3** Cross-node sharing requires explicit user consent via approval queue

---

## Feature 7: Context Compression

**Problem:** No iterative summarization when approaching context limits. Token counting exists but no compression. Highest-risk feature because it deliberately discards information.

**Key files:** `lifecycle.rs` (session execution loop), checkpoint system

**Autonoetic constraints:** Turn continuation state must be restorable exactly, multi-agent child sessions, checkpoint resume.

### Tasks

#### Phase A — ContextState + Summarize Strategy (~300 lines)

- [ ] **7A.1** Define `ContextState` struct (messages, summary, archived_turns, checkpoint_handle, token_budget, compression_threshold)
- [ ] **7A.2** Add `compression_llm_preset` config field (always use cheap model)
- [ ] **7A.3** Implement summarize flow: identify compressible range (all except last N), LLM summarization with structured output (goals, decisions, open_items, key_facts), replace range with summary message
- [ ] **7A.4** Write full compressed context to content store for audit/restore
- [ ] **7A.5** Make compression opt-in per agent via SKILL.md manifest

#### Phase B — Archive Strategy (~100 lines)

- [ ] **7B.1** For suspended sessions: full context preserved on disk, only summary in memory
- [ ] **7B.2** On resume: reload from checkpoint (no compression needed)

#### Phase C — Prune Strategy (~100 lines)

- [ ] **7C.1** For child agent context handoff: parent summarizes what child needs, discards rest
- [ ] **7C.2** Wire into `agent.spawn` delegation flow

#### Phase D — Quality Regression Framework (~400 lines)

- [ ] **7D.1** Record golden sessions as JSON fixtures
- [ ] **7D.2** Replay executor: run same session with/without compression (extends existing FixedTextDriver test pattern)
- [ ] **7D.3** Structural comparison: compare tool call sequences, decisions, final output shape
- [ ] **7D.4** Threshold scanning: test compression at different trigger points

---

## Cross-Cutting Concerns

These appear in multiple features and should be considered during implementation:

- **Causal chain logging:** Features 1, 2, 4 all add new event types to the causal chain
- **Config parsing:** Features 1, 2 add new `gateway.yaml` sections
- **Capability system:** Features 3, 5, 6 add new capability types
- **Approval queue:** Features 2, 3, 6 add new approval request types
- **Content store:** Features 4, 5, 7 use the content-addressed store for new purposes
- **Federation metadata:** Features 4, 6 add `origin_node_id` / `trust_domain` fields
- **Cheap model infrastructure:** Features 2, 4E, 7 all need a configured cheap/fast LLM for gateway-side tasks

## Notes

- All features preserve Autonoetic's separation of powers — agents propose, gateway enforces
- Multi-user, approval gates, and cross-node provenance are baked into every design from the start
- Each feature has an MVP slice that can ship independently
- No behavior changes to existing tools or execution paths
