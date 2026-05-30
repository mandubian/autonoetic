# LLM Context Overflow Mitigation Plan

## Status

**Implemented and migrated to default** — all 5 phases shipped across PRs
#216, #217, #218, #219, #226. The three rollout feature flags have since
been removed and the governor pipeline (with the capsule strategy as the
default LLM-tier reducer) is the only enforcement path. Constitution rule
P-6.9 was updated accordingly in version `2026.05.19` (living constitution: **2026.05.30**).

| Phase | What | PR |
|-------|------|----|
| Phase 0–1 | Safe config defaults + Pluggable Context Governor module | #216 |
| Phase 3 | Overflow-aware retry classifier + context_overflow error class | #217 |
| Phase 4 | SKILL.md split marker convention (`<!-- extended -->`) | #218 |
| Phase 2 | Hierarchical session summarization (CapsuleStrategy) | #219 |
| Phase 5 | TUI transparency for context health | #226 |

> **Historical record:** the sections below describe the **original rollout
> plan**. References to `AUTONOETIC_STRICT_CONTEXT_GOVERNOR`,
> `AUTONOETIC_OVERFLOW_RETRY_CLASSIFIER`, `AUTONOETIC_STATE_CAPSULE_COMPRESSION`,
> the legacy `apply_prompt_budget` path, and the `CompressionStrategy` LLM-tier
> reducer reflect that plan, not the current code. PR #227 removed all three
> flags and the legacy path; capsule is now the only LLM-tier reducer. The
> design rationale is preserved for posterity.

## Incident Summary

A workflow run failed in the promotion/install path with:

- `Context size has been exceeded` (OpenAI-compatible 500)
- Followed by JSON-RPC client disconnect (`Broken pipe`)

The observed token trajectory in the same run reached very high input sizes (roughly 42k to 50k+), and failure occurred during an async child task retry path, which reduced operator visibility and made progress appear stalled.

## Goals

1. Prevent hard context-overflow failures in long-running workflows.
2. Keep workflow progress visible and understandable when compression/retry is happening.
3. Avoid redundant child respawns after a successful stage when later turns overflow.
4. Preserve auditability and correctness while compressing context.

## Non-Goals

1. Rewriting agent instruction architecture end-to-end in this iteration.
2. Changing constitutional policy semantics.
3. Optimizing model quality/cost beyond context safety requirements.

## Existing Building Blocks (Already in Repo)

These are implemented and tested. The plan consolidates them behind a pluggable interface.

| Building Block | Location | Notes |
|---|---|---|
| Token estimation (`estimate_tokens`, heuristic 4 chars/token) | `prompt_budget.rs:79-86` | Per-tool overhead at L14 |
| Budget breakdown computation (`PromptBudgetBreakdown`) | `prompt_budget.rs:20-77` | Sums system + conversation + tool tokens |
| Enforcement strategies: Warn, TrimHistory, DemoteTools, Fail | `prompt_budget.rs:160-440` | Strategy trait at L146; factory at L429 |
| Per-section caps (system_prompt_max_tokens, tool_definitions_max_tokens) | `budget_tracker.rs:191-220` | `check_section_caps()` |
| Context compression with LLM summarization | `compression.rs:239-422` | Tool-call group preservation at L134-205 |
| Compression metadata in checkpoints | `checkpoint.rs:153-155` | Restored on session resume at L159-169 |
| Per-agent compression overrides via SKILL.md | `agent.rs:289-301` (`CompressionConfig`) | |
| Schema compression after turn 0 | `prompt_budget.rs:116-136` | |
| OpenRouter catalog-based context window resolution | `budget_tracker.rs:112-132` | `resolve_context_window_for_run()` |
| Config types: `PromptBudgetConfig`, `ContextCompressionConfig` | `config.rs:1738-1842` | Fields exist but commented-out in template |

## Root Causes

1. Context growth was allowed to accumulate across planner + child loops until model hard limit was reached.
2. Context window was not treated as a strict known bound in all paths (observability fields often `None`).
3. Overflow errors were handled as generic task failure, not a dedicated recovery class.
4. Retry/orchestration behavior allowed additional child runs when prior success should have been terminal for that stage.
5. TUI/operator feedback did not clearly surface "overflow recovery in progress" states.

## Architecture: Pluggable Context Governor

### Problem with Current Layout

The existing context management logic is scattered across three files with no unified entry point:

- `prompt_budget.rs` — budget computation and enforcement strategies
- `budget_tracker.rs` — `apply_prompt_budget()` wiring, context window resolution
- `compression.rs` — summarization-based compression

In `lifecycle.rs:1232-1352`, the reasoning loop calls these independently in sequence: compute breakdown, enforce budget, compress context. This means:
- Adding a new reduction strategy requires touching multiple files.
- The cascade (compress → recompute → trim → fail) must be hand-wired at each call site.
- No single place to reason about the full context lifecycle.

### Solution: `ContextGovernor` Module

Introduce a new module `runtime/context_governor/` that owns the entire context budget lifecycle as a single pluggable pipeline. Existing code moves into this module as strategy implementations — the lifecycle loop calls one method.

```
runtime/context_governor/
├── mod.rs              ContextGovernor struct, pipeline orchestration
├── budget.rs           PromptBudgetBreakdown computation (from prompt_budget.rs)
├── strategies.rs       ReductionStrategy trait + built-in implementations
├── compression.rs      CompressionStrategy — wraps existing compression.rs logic
├── trimming.rs         TrimHistoryStrategy — from prompt_budget.rs TrimHistoryStrategy
├── demotion.rs         ToolDemotionStrategy — from prompt_budget.rs DemoteToolsStrategy
├── error.rs            ContextOverflowError, ContextOverflowDiagnostic types
└── resolver.rs         Context window resolution (from budget_tracker.rs)
```

### Core Trait: `ReductionStrategy`

All context reduction approaches implement the same trait:

```rust
pub trait ReductionStrategy: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn reduce(
        &self,
        ctx: &mut GovernorContext,
    ) -> anyhow::Result<ReductionOutcome>;
}

pub struct GovernorContext {
    pub history: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub breakdown: PromptBudgetBreakdown,
    pub effective_limit: usize,
    pub budget_config: PromptBudgetConfig,
    pub compression_config: Option<ContextCompressionConfig>,
    pub compression_metadata: Option<CompressionMetadata>,
}

pub enum ReductionOutcome {
    Resolved { tokens_after: usize },
    Insufficient { tokens_remaining: usize },
}
```

Built-in strategies:
- `CompressionStrategy` — LLM summarization (wraps existing `compress_context()`)
- `TrimHistoryStrategy` — drop oldest message groups (from existing `TrimHistoryStrategy`)
- `ToolDemotionStrategy` — remove specialized-tier tools (from existing `DemoteToolsStrategy`)
- `ToolSchemaCompressionStrategy` — strip schemas after turn 0 (from existing `compress_tool_definitions()`)
- `FailStrategy` — emit typed `ContextOverflowError`

### Pipeline: `ContextGovernor::govern()`

The governor runs strategies in a configurable ordered pipeline. Each strategy runs only if the previous one returned `Insufficient`:

```rust
pub struct ContextGovernor {
    strategies: Vec<Box<dyn ReductionStrategy>>,
    config: GovernorConfig,
}

impl ContextGovernor {
    pub async fn govern(&self, ctx: &mut GovernorContext) -> GovernorResult {
        let breakdown = PromptBudgetBreakdown::compute(...);
        ctx.breakdown = breakdown;

        if ctx.breakdown.total_tokens <= ctx.effective_limit {
            return GovernorResult::WithinBudget;
        }

        let mut actions_taken: Vec<GovernorAction> = Vec::new();

        for strategy in &self.strategies {
            let outcome = strategy.reduce(ctx)?;
            match outcome {
                ReductionOutcome::Resolved { tokens_after } => {
                    actions_taken.push(GovernorAction {
                        strategy: strategy.name(),
                        tokens_after,
                    });
                    return GovernorResult::Recovered { actions_taken };
                }
                ReductionOutcome::Insufficient { tokens_remaining } => {
                    actions_taken.push(GovernorAction {
                        strategy: strategy.name(),
                        tokens_after: tokens_remaining,
                    });
                    continue;
                }
            }
        }

        GovernorResult::Overflow(ContextOverflowDiagnostic {
            budget_snapshot: BudgetSnapshot::from(ctx),
            actions_attempted: actions_taken,
            recovery_action: RecoveryAction::Failed,
        })
    }
}
```

### Plugging New Approaches

Adding a new reduction method (e.g., "state capsule" from Phase 2) requires only:

1. Implement `ReductionStrategy` for the new approach.
2. Add it to the strategy pipeline in config or code.

No changes to `lifecycle.rs` or `budget_tracker.rs`.

### Lifecycle Integration

The reasoning loop at `lifecycle.rs:1232-1352` currently has ~120 lines of budget computation, enforcement, and compression. This collapses to:

```rust
let mut gov_ctx = GovernorContext::new(
    history.clone(),
    tools,
    context_window_resolved,
    &budget_config,
    compression_cfg.cloned(),
    self.compression_metadata.clone(),
);
let result = self.context_governor.govern(&mut gov_ctx).await;
match result {
    GovernorResult::WithinBudget => {}
    GovernorResult::Recovered { actions_taken } => {
        tracer.log_event("context_governor", "recovered", Success, ...);
        *history = gov_ctx.history;
    }
    GovernorResult::Overflow(diag) => {
        tracer.log_event("context_governor", "overflow", Error, ...);
        return Err(ContextOverflowError::new(diag).into());
    }
}
```

### Strategy Ordering

Default pipeline order (configurable per profile):

1. `ToolSchemaCompression` — if turn > 0, strip schemas (fast, lossless)
2. `Compression` — LLM summarization (slow, preserves semantics)
3. `TrimHistory` — drop oldest groups (fast, lossy)
4. `ToolDemotion` — remove specialized tools (fast, reduces capability)
5. `Fail` — typed overflow error (terminal)

Each step only runs if the previous returned `Insufficient`.

### Feature Flag Integration

The governor checks feature flags to select pipeline:

- `AUTONOETIC_STRICT_CONTEXT_GOVERNOR=1` — use full cascade pipeline above
- Flag unset — use legacy behavior (warn-only, no cascade) for backward compatibility

### Migration Plan for Existing Code

| Current Code | Destination | Change |
|---|---|---|
| `prompt_budget::PromptBudgetBreakdown` | `context_governor::budget::PromptBudgetBreakdown` | Move, re-export from old location with deprecation |
| `prompt_budget::BudgetEnforcementStrategy` trait | Removed | Replaced by `ReductionStrategy` |
| `prompt_budget::WarnStrategy` | `context_governor::WarnOnlyStrategy` | Wraps old behavior |
| `prompt_budget::TrimHistoryStrategy` | `context_governor::trimming::TrimHistoryStrategy` | Move, implement `ReductionStrategy` |
| `prompt_budget::DemoteToolsStrategy` | `context_governor::demotion::ToolDemotionStrategy` | Move, implement `ReductionStrategy` |
| `prompt_budget::FailStrategy` | `context_governor::FailStrategy` | Move, emit `ContextOverflowError` |
| `prompt_budget::compress_tool_definitions()` | `context_governor::ToolSchemaCompressionStrategy` | Wrap as strategy |
| `compression::compress_context()` | `context_governor::compression::CompressionStrategy` | Wrap as strategy |
| `budget_tracker::apply_prompt_budget()` | Removed | Replaced by `ContextGovernor::govern()` |
| `budget_tracker::resolve_context_window_*()` | `context_governor::resolver::*` | Move |
| `budget_tracker::is_retryable_empty_other_response()` | Stays in `budget_tracker.rs` | Not context-governor concern |

Old modules re-export moved types with `#[deprecated]` attributes during migration. Remove re-exports once all call sites are updated.

## Plan

## Phase 0: Safe Defaults (Configuration)

Uncomment and set safe defaults in `config/config-template.yaml` (fields already exist at lines 307-316, 485-493 but are commented out):

```yaml
prompt_budget:
  warn_at_pct: 75.0
  margin_tokens: 4096
  on_exceeded: trim_history
  compress_tool_schemas_after_turn_0: true

context_compression:
  enabled: true
  llm_preset: haiku
  threshold_pct: 60.0
  recent_turns_to_keep: 3
  max_summary_tokens: 500
  min_turns_between_compression: 2
```

Also require explicit context-window declaration for local OpenAI-compatible backends used by planner/factory/builder profiles (either in preset/manifest or via runtime env fallback).

Deliverables:

1. Config profile update in template/reference docs.
2. Runtime warning on startup when context window is unknown for active preset (fire only when all resolution paths fail: manifest, env var, and provider catalog. The existing `resolve_context_window_for_run()` at `budget_tracker.rs:112-132` already has catalog fallback for OpenRouter — do not warn when catalog resolves successfully).
3. Add telemetry field `system_prompt_tokens` from `PromptBudgetBreakdown` (already computed at `prompt_budget.rs:38-77`) to enable measurement before Phase 4 investment.

## Phase 1: Context Governor Module

### 1a. Scaffold the Module

Create `runtime/context_governor/` with the trait, types, and pipeline from the architecture section above. Initially populate with thin wrappers around existing code:

- `budget.rs` — move `PromptBudgetBreakdown` and `estimate_tokens`
- `strategies.rs` — `ReductionStrategy` trait, `GovernorContext`, `ReductionOutcome`
- `compression.rs` — `CompressionStrategy` wrapping `compress_context()`
- `trimming.rs` — `TrimHistoryStrategy` from existing code
- `demotion.rs` — `ToolDemotionStrategy` from existing code
- `error.rs` — `ContextOverflowError`, `ContextOverflowDiagnostic`, `BudgetSnapshot`
- `resolver.rs` — context window resolution functions
- `mod.rs` — `ContextGovernor` struct with configurable pipeline

### 1b. Wire into Lifecycle

Replace the 120-line block at `lifecycle.rs:1232-1352` with a single `ContextGovernor::govern()` call. The governor is constructed once per `AgentExecutor` and stored as a field.

### 1c. Provider-Side Error Classification

Parse provider-specific overflow errors in LLM drivers and return typed `ContextOverflowError`:

- `openai.rs`: match `context_length_exceeded` error code
- `anthropic.rs`: match `max_context_window_reached` error code
- `gemini.rs`: match `RESOURCE_EXHAUSTED` with context details

Currently all return generic `anyhow::bail!()` at:
- `openai.rs:213`
- `anthropic.rs:148`
- `gemini.rs:198`

### 1d. Deprecation Shims

Add `#[deprecated]` re-exports in `prompt_budget.rs` and `budget_tracker.rs` pointing to `context_governor::*`. This lets other call sites migrate incrementally.

Deliverables:

1. `runtime/context_governor/` module with trait, pipeline, and all built-in strategies.
2. Lifecycle integration replacing `lifecycle.rs:1232-1352`.
3. `ContextOverflowError` and `ContextOverflowDiagnostic` in `autonoetic-types` + `context_governor::error`.
4. Provider error classification in all three LLM drivers.
5. Deprecation shims in old modules.
6. Unit tests: strategy isolation (each strategy works independently), pipeline ordering (cascade stops at first `Resolved`), fallback (all strategies exhausted → typed error).

## Phase 2: Hierarchical Session Summarization (CapsuleStrategy)

> **Note**: This phase is deferred after Phases 0, 1, and 3 ship. The existing `CompressionStrategy` in the governor is sufficient for the immediate failure mode. Because the governor is pluggable, the capsule ships as a new `ReductionStrategy` implementation with zero changes to the pipeline or lifecycle.

> **Feature flag**: `AUTONOETIC_STATE_CAPSULE_COMPRESSION=1`

### Motivation

The existing `CompressionStrategy` performs full LLM-based summarization of old turns on each compression cycle. This works for the immediate failure mode but has structural limitations in long-running (50+ turn) sessions:

1. **Repeated re-summarization**: Each compression cycle regenerates the summary from scratch, losing incremental context nuance.
2. **Structural information loss**: The flat summary text doesn't preserve structured identifiers (artifact handles, approval IDs, revision IDs) with guaranteed losslessness.
3. **No semantic layering**: A planner session at turn 80 carries the same flat summary block as at turn 15 — there's no hierarchical decomposition of what matters.
4. **Quality degradation**: Each re-summarization compounds error, especially for decisions and rationale that drift across compression cycles.

### Concept

Introduce a durable, versioned **state capsule** that maintains rolling structured sections updated from deltas rather than regenerated from full history:

1. Keep last N turns raw (N=3 default, reusing existing `split_compressible_messages()`).
2. Maintain rolling structured sections (see schema below).
3. On each compression cycle, extract a typed `CapsuleDelta` from recent turns via LLM, then apply it mechanically.
4. Preserve original capsule versions in content store for audit/replay (immutable chain).

### Capsule Data Model

The capsule is a versioned, structured state object at `context_governor::capsule`:

```rust
/// A durable state capsule that maintains rolling structured summaries.
///
/// Unlike flat compression, the capsule preserves structured sections
/// that are updated incrementally from deltas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateCapsule {
    /// Monotonic version counter, incremented on each update.
    pub version: u64,
    /// Session that owns this capsule.
    pub session_id: String,
    /// Turn number when this capsule was last updated.
    pub last_update_turn: u64,

    // --- Structured Sections ---

    /// Current objective and success criteria.
    /// Updated when the agent's goal shifts or refines.
    pub objective_and_criteria: String,
    /// Key decisions made and their rationale.
    /// Append-only: new decisions are appended, never removed.
    pub decisions_and_rationale: Vec<CapsuleDecision>,
    /// Stable identifiers that MUST be preserved losslessly.
    /// Content handles, artifact refs, approval IDs, revision IDs.
    pub stable_identifiers: Vec<StableIdentifier>,
    /// Open tasks and blockers at the current point.
    /// Mutable: tasks are added, marked complete, or removed.
    pub open_tasks: Vec<CapsuleTask>,

    // --- Audit chain ---

    /// Content store handle to the previous capsule version.
    pub previous_version_handle: Option<String>,
    /// Content store handle to the full original history snapshot
    /// that was compressed into this capsule version.
    pub source_history_handle: Option<String>,
    /// Timestamp of last update (RFC3339).
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleDecision {
    pub turn: u64,
    pub summary: String,
    pub rationale: String,
    pub referenced_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct StableIdentifier {
    /// Category: "artifact", "approval", "revision", "content", "session", etc.
    pub category: String,
    /// The identifier value (content handle, approval ID, etc.)
    pub value: String,
    /// Human-readable label (optional).
    pub label: Option<String>,
    /// Turn when this ID was first referenced.
    pub first_seen_turn: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleTask {
    pub description: String,
    /// Status: "open", "in_progress", "blocked", "completed".
    pub status: String,
    pub added_turn: u64,
    pub completed_turn: Option<u64>,
    pub blocker: Option<String>,
}
```

**Design rationale**: Structured sections rather than free-form text. The LLM extracts deltas into typed fields; the gateway mechanically enforces that `stable_identifiers` are never mutated (only appended). This follows the "dumb gateway" philosophy: the LLM does the semantic work (extraction), the gateway does the mechanical work (enforcement).

### Delta Extraction

On each compression cycle, the `CapsuleStrategy`:

1. Sends the current capsule state (JSON) + recent compressible turns + a `CapsuleDelta` schema description to the compression LLM.
2. Receives a typed `CapsuleDelta` as JSON.
3. Validates the delta mechanically (see safety invariants below).
4. Applies the delta to produce a new `StateCapsule` with `version + 1`.
5. Persists the previous version to the content store (audit chain).

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleDelta {
    /// Updated objective (if changed), or None to keep current.
    pub objective_update: Option<String>,
    /// New decisions to append.
    pub new_decisions: Vec<CapsuleDecision>,
    /// New stable identifiers discovered in recent turns.
    pub new_identifiers: Vec<StableIdentifier>,
    /// Task updates: new tasks, status changes, completions.
    pub task_updates: Vec<CapsuleTaskUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapsuleTaskUpdate {
    Add(CapsuleTask),
    Complete { description: String, turn: u64 },
    Block { description: String, blocker: String },
    Remove { description: String },
}
```

### CapsuleStrategy — ReductionStrategy Implementation

`CapsuleStrategy` implements `ReductionStrategy` and replaces `CompressionStrategy` in the pipeline when the feature flag is set:

```rust
pub struct CapsuleStrategy {
    http_client: reqwest::Client,
    presets: HashMap<String, LlmPreset>,
    gateway_dir: Option<PathBuf>,
}

#[async_trait]
impl ReductionStrategy for CapsuleStrategy {
    fn name(&self) -> &'static str { "capsule" }

    async fn reduce(&self, ctx: &mut GovernorContext) -> anyhow::Result<ReductionOutcome> {
        // 1. Check capsule compression enabled + LLM configured
        // 2. Split history via split_compressible_messages() (reuse existing)
        // 3. Load or create capsule from GovernorContext
        // 4. Extract CapsuleDelta from compressible turns via LLM
        // 5. Validate delta (no ID mutations, no decision removals,
        //    unresolved approvals preserved)
        // 6. Apply delta → new capsule version (version + 1)
        // 7. Persist previous version to content store
        // 8. Replace compressible history with capsule injection message
        // 9. Update ctx.history, ctx.compression_metadata
        // 10. Recompute token estimate; return Resolved or Insufficient
    }
}
```

**Pipeline position**: With the feature flag enabled, the pipeline becomes:

1. `ToolSchemaCompression` — strip schemas after turn 0 (fast, lossless)
2. **`Capsule`** — delta-based structured compression (replaces `Compression`)
3. `TrimHistory` — drop oldest groups (fast, lossy)
4. `ToolDemotion` — remove specialized tools (fast, reduces capability)
5. `Fail` — typed overflow error (terminal)

`CompressionStrategy` remains available as the default when the flag is unset.

### Capsule Injection Format

Instead of the current `[COMPRESSED CONTEXT - Turn N]` flat text, the capsule injects a structured message:

```
[SESSION STATE CAPSULE v{version} — Turn {turn}]

## Objective
{objective_and_criteria}

## Key Decisions
- [Turn {turn}] {summary}: {rationale}
...

## Active Identifiers
- [{category}] {value} ({label})
...

## Open Tasks
- [{status}] {description}
...

## Completed Tasks (Recent)
- [done@{turn}] {description}
...
```

### Safety Invariants (Mechanical Enforcement)

These are enforced by the gateway, not by the LLM:

| Invariant | Enforcement |
|---|---|
| Stable identifiers are never mutated | `apply_delta()` rejects any `new_identifiers` that conflict with existing entries by `(category, value)` |
| Decisions are append-only | `apply_delta()` only appends `new_decisions`; no mechanism to modify or remove |
| Unresolved approvals survive compression | `validate_delta()` scans compressed turns for approval tool calls without matching `approval.status` results; rejects delta if these aren't in `open_tasks` or `stable_identifiers` |
| Capsule version chain is immutable | Each update persists the previous version to content store; content store blobs are read-only |
| Most recent tool-call group is kept intact | Reuses existing `split_compressible_messages()` logic — tool-call groups are never split |
| Decision overflow bounded | Capsule retains at most `max_capsule_decisions` (default 30); oldest beyond limit are summarized into a single "prior decisions summary" entry |
| Completed task pruning | At most `max_completed_tasks` (default 10) completed tasks retained; oldest are dropped |

### Persistence

#### Content Store

- Previous capsule versions → `content_store.write()` with name `capsule_v{version}_turn_{turn}`, visibility `Private` (session-local for v1)
- Source history snapshots → `content_store.write()` (same as existing `CompressionStrategy`)
- Audit chain: each capsule's `previous_version_handle` points to the prior version's content store handle

#### Checkpoint

`SessionCheckpoint` gains a new field:

```rust
// --- Capsule ---
#[serde(default, skip_serializing_if = "Option::is_none")]
pub capsule_state: Option<StateCapsule>,
```

On session resume, `restore_into()` restores capsule state so the next compression cycle does a delta update rather than starting fresh. `AgentExecutor` stores the capsule as `pub capsule_state: Option<StateCapsule>`.

### Configuration

`ContextCompressionConfig` gains capsule-specific fields:

```rust
/// Compression strategy: "summarization" (default) or "capsule".
#[serde(default = "default_compression_strategy")]
pub strategy: String,
/// Maximum decisions retained in capsule (default 30).
#[serde(default = "default_max_capsule_decisions")]
pub max_capsule_decisions: usize,
/// Maximum completed tasks retained in capsule (default 10).
#[serde(default = "default_max_completed_tasks")]
pub max_completed_tasks: usize,
```

`CompressionConfig` (per-agent SKILL.md) gains optional overrides:

```rust
/// Override compression strategy for this agent: "summarization" or "capsule".
pub strategy: Option<String>,
/// Override max capsule decisions for this agent.
pub max_capsule_decisions: Option<usize>,
```

### Capsule Scope (Concurrency Decision)

v1: **session-local only**. Each session owns its own capsule — no cross-session sharing, no CAS/locking needed. This avoids the concurrency complexity of two sibling sessions compressing simultaneously into a shared capsule.

Future: if telemetry shows benefit, upgrade to root-session-shared capsules with CAS-style optimistic locking on the capsule version.

### Migration from CompressionStrategy

When upgrading mid-session (operator enables the feature flag during a running session), the `CapsuleStrategy` detects existing `[COMPRESSED CONTEXT]` markers in history and bootstraps the capsule's `objective_and_criteria` section from their content. The first capsule version is created at `version: 1` with the bootstrapped objective and empty decisions/identifiers/tasks sections.

### Deliverables

1. `context_governor::capsule::CapsuleStrategy` implementing `ReductionStrategy`.
2. `StateCapsule`, `CapsuleDelta`, `StableIdentifier`, `CapsuleDecision`, `CapsuleTask` types with serialization.
3. `apply_delta()` with mechanical enforcement of safety invariants.
4. `validate_delta()` for unresolved approval detection.
5. Delta extraction prompt and LLM integration (reusing compression LLM config).
6. Content store persistence for capsule version chain.
7. `SessionCheckpoint.capsule_state` field + `restore_into()` integration.
8. Config entries in `ContextCompressionConfig` and `CompressionConfig`.
9. Feature gate: `AUTONOETIC_STATE_CAPSULE_COMPRESSION=1` selects `CapsuleStrategy` in pipeline.
10. Migration logic: bootstrap capsule from existing `[COMPRESSED CONTEXT]` markers.

### Tests

Unit tests (in `capsule.rs`):

1. Delta application: valid delta → version incremented, sections updated.
2. ID preservation invariant: conflicting `StableIdentifier` → rejected.
3. Decision append-only: cannot remove or modify existing decisions.
4. Unresolved approval detection: compressed turns with pending approval calls → delta must preserve them.
5. Capsule injection format: serialized capsule produces expected structured text.
6. Migration from compressed context: `[COMPRESSED CONTEXT]` → bootstrapped capsule.
7. Decision overflow: >30 decisions → oldest summarized into prior-decisions entry.
8. Completed task pruning: >10 completed tasks → oldest dropped.

Property-based tests (quickcheck-style):

1. **ID losslessness**: for any arbitrary history containing structured IDs, after capsule compression, all IDs appear in `stable_identifiers`.
2. **Decision monotonicity**: for any sequence of delta applications, decisions list length is monotonically non-decreasing.
3. **Version monotonicity**: capsule version is strictly monotonically increasing.

Pipeline tests:

1. CapsuleStrategy resolves → `ReductionOutcome::Resolved`.
2. CapsuleStrategy insufficient → `ReductionOutcome::Insufficient`, pipeline continues to `TrimHistory`.
3. Feature flag off → `CompressionStrategy` used instead.
4. Pipeline contains exactly one compression-tier strategy (capsule xor summarization).

Integration tests:

1. 50-turn synthetic session → capsule applied, history reduced, all approval IDs preserved, session resumable from checkpoint.
2. Compress → checkpoint → resume → second compression uses delta on existing capsule (version 2).
3. Content store audit chain: 3 capsule versions → 3 snapshots with correct `previous_version_handle` chain.
4. Migration: session starts with `CompressionStrategy`, flag enabled, next compression bootstraps capsule from existing markers.

## Phase 3: Overflow-Aware Orchestration and Retry

Make scheduler behavior explicit for context overflow:

1. Classify overflow as recoverable-once with compaction.
2. Retry exactly once with `overflow_recovery=true` and forced aggressive pipeline: the retry governor uses a reduced pipeline that skips `CompressionStrategy` (already attempted) and goes straight to `TrimHistory`. The retry must also force `compress_tool_schemas_after_turn_0: true` regardless of original config.
3. If second attempt overflows, mark task terminal with class `context_overflow_terminal`.
4. Do not spawn sibling/duplicate builder tasks if install stage already reached success for same `(agent_id, revision_id)` tuple.
5. Enforce idempotent stage transitions with a `stage_transitions` table in gateway SQLite keyed on `(agent_id, revision_id, stage)` with a `UNIQUE` constraint. This survives restarts unlike an in-memory lock. Follow the existing `gateway_store` migration pattern — increment `SCHEMA_VERSION_LATEST` and add an `apply_*_vN()` function.
   - `install_requested`
   - `install_succeeded`
   - `promoted`
6. Record `overflow_recovery_attempted: bool` in the task checkpoint (currently only `"status": "failed"` as a string at `scheduler.rs:1440`).

Deliverables:

1. Retry policy update for overflow class.
2. Idempotency guard for builder/install stage (SQLite-backed).
3. Regression test covering "first builder success + second overflow should not trigger duplicate install path".
4. Overflow retry coordination through gateway store for concurrent sibling tasks (two builders overflowing simultaneously must not independently retry into the same stage transition).

## Phase 4: Prompt Diet for High-Churn Agents

> **Note**: Requires measurement data from Phase 0 deliverable 3 (`system_prompt_tokens` telemetry) before implementation. Do not invest until baseline is established.

Reduce recurring token overhead for planner/factory/builder:

1. Split SKILL guidance into:
   1. Core runtime instructions (always injected).
   2. Extended examples/reference (on-demand retrieval).
2. Keep strict rules and contracts in core; move verbose examples to optional retrieval content.
3. Enable tool schema demotion after turn 0 by default in these profiles.

Deliverables:

1. Instruction profile convention doc.
2. Pilot reduction target: 25-40% system-token reduction for planner-like agents (validated against telemetry).

## Phase 5: Operator/TUI Transparency

Expose explicit context-health states in UI/events. Wire into the existing causal event chain, not a separate logging path.

1. `context_pressure_high` warning with percentages and top contributors.
2. `context_compression_applied` event with before/after token estimates.
3. `overflow_retry_started` and `overflow_retry_exhausted` events.
4. Session card badge when context window is unknown.

Deliverables:

1. Event types + renderers.
2. TUI status line and workflow card updates.

## StopReason::MaxTokens Recovery

Currently `StopReason::MaxTokens` at `lifecycle.rs:2394` just breaks the loop. For multi-turn sessions that re-enter the agent, this should trigger the governor's pipeline before the next turn, not silently discard the partial response. Track as a Phase 1 follow-up.

## Context Window Resolution for Non-OpenRouter Providers

The existing `resolve_context_window_for_run()` at `budget_tracker.rs:112-132` has catalog fallback only for OpenRouter. Anthropic and Gemini model context sizes need a static lookup table (either hardcoded or in config) so the governor can enforce budgets for all providers. After Phase 1, this logic lives in `context_governor::resolver`.

## Data Model and Telemetry Changes

Add/extend fields in session/workflow telemetry:

1. `context_window_tokens_effective` (required at dispatch time).
2. `estimated_input_tokens_preflight`.
3. `estimated_input_tokens_post_compression`.
4. `compression_ratio`.
5. `overflow_recovery_attempt` (bool).
6. `overflow_error_class`.
7. `system_prompt_tokens` (from `PromptBudgetBreakdown`, for Phase 4 measurement).
8. `governor_actions_taken` (ordered list of strategy names that fired, with tokens after each).

## Test Plan

1. Unit tests (per-strategy isolation):
   1. `CompressionStrategy` — threshold behavior, tool-call group preservation.
   2. `TrimHistoryStrategy` — oldest-first removal, tool-call group integrity, message floor.
   3. `ToolDemotionStrategy` — tier filtering, section cap compliance.
   4. `ToolSchemaCompressionStrategy` — turn-0 skip, subsequent-turn compression.
   5. `FailStrategy` — emits `ContextOverflowError` with diagnostic.
   6. `CapsuleStrategy` — delta application, ID preservation invariant, decision append-only, unresolved approval detection, capsule injection format, migration from `[COMPRESSED CONTEXT]`, decision overflow (>30 → summarized), completed task pruning (>10 → dropped).
2. Pipeline tests (cascade behavior):
   1. Within budget → no strategies run.
   2. Single strategy resolves → later strategies skipped.
   3. All strategies exhausted → `GovernorResult::Overflow` with full diagnostic.
   4. Custom pipeline order via config.
   5. Capsule replaces `CompressionStrategy` when `AUTONOETIC_STATE_CAPSULE_COMPRESSION=1` is set; pipeline has exactly one compression-tier strategy.
3. Property-based tests (capsule invariants):
   1. ID losslessness: for arbitrary history with structured IDs, all IDs appear in `stable_identifiers` after capsule compression.
   2. Decision monotonicity: decisions list length is monotonically non-decreasing across any delta application sequence.
   3. Version monotonicity: capsule version is strictly monotonically increasing.
4. Integration tests:
   1. Simulated long-history planner run that would previously exceed context; verify no hard failure.
   2. Overflow retry path emits expected events and terminal classification.
   3. Builder duplicate-spawn prevention after successful promotion/install tuple.
   4. Concurrent sibling overflow: both children overflow, only one retry proceeds, no duplicate stage transition.
   5. Provider error classification (parse OpenAI/Anthropic/Gemini overflow error codes → `ContextOverflowError`).
   6. 50-turn synthetic session with capsule: history reduced, approval IDs preserved, session resumable from checkpoint.
   7. Capsule across session resume: compress → checkpoint → resume → delta update (version 2).
   8. Content store audit chain: 3 capsule versions → 3 snapshots with correct `previous_version_handle` chain.
   9. Migration: session starts with `CompressionStrategy`, flag enabled mid-session, next compression bootstraps capsule.
5. Soak test:
   1. Long-running multi-agent session (50+ turns) with approvals and workflow joins.

## Rollout Strategy

**Shipped first: Phases 0, 1, and 3** — these directly fix the observed failure mode.

1. Stage A: Config-only rollout (Phase 0) behind environment profile.
2. Stage B: Scaffold `context_governor` module, migrate existing strategies behind `ReductionStrategy` trait, wire into lifecycle. Feature-gated behind `AUTONOETIC_STRICT_CONTEXT_GOVERNOR=1`.
3. Stage C: Enable Phase 3 overflow retry classifier under `AUTONOETIC_OVERFLOW_RETRY_CLASSIFIER=1`.
4. Stage D: Enable Phase 2 capsule in canary. RFC complete (see Phase 2 section above). Ships as `CapsuleStrategy` implementing `ReductionStrategy` behind `AUTONOETIC_STATE_CAPSULE_COMPRESSION=1`. Session-local scope for v1 (no cross-session sharing). Swaps `CompressionStrategy` in the pipeline when enabled.
5. Stage E: Turn on Phase 5 UI transparency by default.

Feature flags:

1. `AUTONOETIC_STRICT_CONTEXT_GOVERNOR=1` — enable governor pipeline (replaces legacy `apply_prompt_budget` + inline compression)
2. `AUTONOETIC_OVERFLOW_RETRY_CLASSIFIER=1` — enable overflow retry in scheduler
3. `AUTONOETIC_STATE_CAPSULE_COMPRESSION=1` — swap `CompressionStrategy` for `CapsuleStrategy` in pipeline

Rollback: to disable the governor in production, unset `AUTONOETIC_STRICT_CONTEXT_GOVERNOR`. The lifecycle falls back to the existing `apply_prompt_budget()` + inline compression path (pre-existing behavior). The old code stays behind the feature flag until the governor is validated in production, then it is removed.

## Acceptance Criteria

1. No unclassified context-overflow 500s in planner/factory/builder paths for benchmark workflows.
2. At most one overflow-recovery retry per failed task.
3. No duplicate install/promotion tasks for the same revision once success recorded.
4. Operators can see compression/retry status in TUI without consulting raw logs.
5. Regression suite includes at least one previously failing trace replay that now completes.
6. Provider-specific overflow errors (OpenAI, Anthropic, Gemini) are classified as `context_overflow`, not generic API errors.
7. Adding a new reduction strategy requires implementing `ReductionStrategy` and adding to config — no changes to `lifecycle.rs` or the pipeline orchestrator.

## Risks and Mitigations

1. Over-compression can remove critical nuance.
   - Mitigation: preserve recent turns + tool groups, keep audit snapshots, add id-preservation tests.
2. Strict governor may increase deterministic failures if misconfigured.
   - Mitigation: startup validation and explicit diagnostics with actionable recovery hints.
3. Added orchestration guards may block legitimate retries.
   - Mitigation: key retries on `(stage, agent_id, revision_id)` and expose override path for manual intervention.
4. Synchronous cascade in Phase 1 adds latency before every LLM call.
   - Mitigation: cascade only triggers when budget is exceeded (fast path is unchanged). Measure compression latency in soak test.
5. State capsule (Phase 2) introduces concurrency complexity.
   - Mitigation: deferred to separate RFC. Ships as isolated `ReductionStrategy` — no pipeline changes needed.
6. Module migration may break callers depending on old `prompt_budget` / `budget_tracker` entry points.
   - Mitigation: `#[deprecated]` re-exports with clear migration paths. Feature flag gates the new path; old path is default until flag is set.

## Immediate Follow-up Tasks

1. Uncomment safe defaults in `config/config-template.yaml` (lines 307-316, 485-493).
2. Scaffold `runtime/context_governor/` module with `ReductionStrategy` trait and `ContextGovernor` pipeline.
3. Move existing strategies behind `ReductionStrategy` with deprecation shims.
4. Wire `ContextGovernor::govern()` into lifecycle behind `AUTONOETIC_STRICT_CONTEXT_GOVERNOR` feature flag.
5. Add `ContextOverflowError` and `ContextOverflowDiagnostic` in `autonoetic-types` + `context_governor::error`.
6. Implement provider-side error classification in OpenAI, Anthropic, and Gemini drivers.
7. Add an integration test fixture reproducing the observed overflow trace.
8. Implement stage idempotency guard for specialized builder promotion/install transitions (SQLite `stage_transitions` table with gateway store migration).
9. Add static context window lookup table for Anthropic and Gemini models (in `context_governor::resolver`).
10. Add `system_prompt_tokens` telemetry field for Phase 4 baseline measurement.
