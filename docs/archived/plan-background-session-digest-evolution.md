# Plan: Background session-digest + agent-evolution + admin-reporting pipeline

## Context

Autonoetic already records rich per-session artifacts — causal chains, digests,
execution traces, causal events, published reports, approval decisions — and it
already has a scaffolded evolution domain (`agent-factory.default`,
`specialized_builder.default`, plus planned-but-not-built `memory-curator.default`
and `evolution-steward.default`). What is missing is the **recurring, cross-cutting
loop** that:

1. Mines completed sessions for patterns
2. Uses those patterns to decide which agents should be evolved (and triggers the
   existing evolution pipeline behind its existing gates)
3. Surfaces gaps that cannot be fixed by agent tuning alone as structured
   proposals for a human admin (or admin-agent) to triage

This plan describes that loop. It is deliberately **agent-bundle-heavy and
Rust-light** — the gateway-side additions are limited to one SQL table, two
native tools, and a migration. Everything else is SKILL.md instructions.

## Goals

- **Cross-session learning**: distilled, durable learnings in the knowledge store
  instead of per-session digests that stay siloed.
- **Closed-loop agent improvement**: underperforming agents get a chance to
  evolve without manual operator intervention, but never without the existing
  evaluator + auditor gates firing.
- **Admin visibility**: gaps that require human judgment (new capabilities, new
  tools, protocol changes) surface as a reviewable queue, not as noise buried in
  logs.
- **Small surface area**: no new top-level architectural concepts. Reuse
  scheduler, knowledge store, agent-factory pipeline, approval queue patterns,
  notifications table.

## Non-goals

- Building a new review UI for proposals. The proposals live in SQLite; CLI
  listing + existing notifications are sufficient for now.
- Replacing `post_session_digest` or `session_report` pipelines. This plan runs
  **on top of** them — it consumes their outputs, it does not rewrite them.
- Evolving foundational agents (`planner.default`, `coder.default`, etc.)
  autonomously. The MVP explicitly excludes these.

---

## Architecture

### Data flow

```
[scheduler tick — cron every 4h]
        │
        ▼
┌──────────────────────────────────────────┐
│  evolution-orchestrator.default          │  NEW root orchestrator
│                                          │
│  1. knowledge_recall("evolution.         │
│       high_water_mark") → last ts        │
│                                          │
│  2. session_search(status=completed,     │
│       since=last_ts) → session_ids       │
│                                          │
│  3. spawn memory-curator.default ────────┼──► digests learnings, scores agents
│     (session_ids, max_sessions)          │   returns { agent_scores, systemic_gaps }
│                                          │
│  4. for each flagged agent:              │
│     spawn evolution-steward.default ─────┼──► decides evolve/skip
│       (agent_id, evidence)               │   spawns agent-factory.default if evolve
│                                          │   (existing gates fire: eval + audit)
│                                          │
│  5. for each systemic gap:               │
│     admin_proposal_create ───────────────┼──► admin_proposals table + notification
│                                          │
│  6. knowledge_store(                     │
│     "evolution.high_water_mark",         │
│     new_ts)  (only on full success)      │
└──────────────────────────────────────────┘
```

### Agents involved

| Agent | Role | Status |
|-------|------|--------|
| `evolution-orchestrator.default` | Root cron-driven orchestrator of the loop | **NEW** |
| `memory-curator.default` | Distills cross-session learnings + scores agents | **NEW** (referenced in `docs/AGENTS.md` but no directory yet) |
| `evolution-steward.default` | Decides evolve/skip per-agent and triggers factory | **NEW** (referenced in `docs/AGENTS.md` but no directory yet) |
| `agent-factory.default` | End-to-end agent creation pipeline | Existing |
| `evaluator.default` | Produces `promotion_record(pass=true)` evidence | Existing |
| `auditor.default` | Produces `promotion_record(pass=true)` evidence | Existing |
| `specialized_builder.default` | Installs revisions (revision.create + promote) | Existing |

### Separation of powers

- Orchestrator holds **no** `AgentRevision` capability. It cannot create or
  promote revisions directly. All revision work is delegated to `agent-factory`.
- Steward holds no `AgentRevision` capability either — same reason.
- Factory pipeline keeps its existing evaluator + auditor gates: the gating
  logic is centralised in `promotion.rs` and `agent.revision.*`, this plan does
  not touch it.

---

## Design decisions

### 1. One agent or a pipeline of agents?

**Decision: pipeline of three.**

Rationale:
- `memory-curator.default` and `evolution-steward.default` are already documented
  in `docs/AGENTS.md` (Evolution Roles table) as distinct responsibilities. This
  plan honours that split.
- Orchestrator owns *when to run, what to process*. Curator owns *what to learn*.
  Steward owns *what to evolve and how*. Each is independently testable and
  independently evolvable.
- A monolithic agent would conflate cadence, analysis, and decision logic — the
  same anti-pattern the planner/specialist split avoids.

Trade-off: 3-5 agent spawns per cron tick. Acceptable at 4h cadence.

### 2. What triggers a run?

**Decision: wall-clock cron (`scheduler_cron_create`) + queue-drain within each run.**

- `BackgroundReevaluation` capability is per-agent periodic wake; wrong fit for
  a cross-cutting orchestrator.
- `scheduled_jobs` table + `process_due_scheduled_jobs` in `scheduler.rs`
  already support targeting a specific agent with a message. Use this.
- Within each run: `session_search(status="completed", since=<bookmark>)` drains
  all new sessions.
- Event-driven (on-session-end) was rejected: fires too often, no batching,
  thundering-herd risk.

Concrete setup: orchestrator's SKILL.md includes a first-run step that calls
`scheduler_cron_create` with `schedule_expr: "0 */4 * * *"` targeting itself with
a predefined message. Idempotent — can be re-run safely.

### 3. How does the orchestrator know what's new?

**Decision: a `knowledge_store` bookmark at id `evolution.high_water_mark`.**

- Stores `{ "last_processed_at": "<rfc3339>", "last_run_id": "<uuid>",
  "generation": <u64> }` with `visibility: "global"`, `retention: "stable"`.
- `session_search(status="completed", since=<last_processed_at>)` returns the
  exact window.
- Crash-safe: bookmark is only updated after all processing succeeds. Partial
  failures re-process the same window on next run (`knowledge_store` upserts by
  id; curator uses deterministic memory ids derived from session hashes — same
  pattern as `digest_memory_id` in `post_session_digest.rs`).
- No new SQL table. No Rust code for the bookmark.

### 4. Selection criterion for agents to evolve

**Decision: a multi-signal score computed by the curator, decided by the steward.**

Signals (in priority order):

| # | Signal | Source | Threshold |
|---|--------|--------|-----------|
| 1 | Failure rate | `execution_traces` WHERE `agent_id=? AND success=false` | > 0.30 |
| 2 | Repeated error patterns | Same `error_type` across ≥ 3 distinct sessions | ≥ 3 occurrences |
| 3 | Approval denial rate | `approvals` WHERE `status='rejected'` GROUP BY `agent_id` | > 2 denials in window |
| 4 | Low evaluator scores | `eval_case_results` via `eval_runs.subject_agent_id` | avg score < 0.5 or `status=Failed` |
| 5 | Escalation frequency | `user_interactions` WHERE `kind='escalation'` | ≥ 2 escalations |
| 6 | Negative post-session-digest memories | `knowledge_search_by_tags(["source:post_session_digest", "type:error_pattern", "agent:<id>"])` | ≥ 3 distinct patterns |

Decision matrix (applied by steward):

- **≥ 3 signals triggered** → immediate evolution candidate
- **exactly 2 signals** → add to `evolution.watch_list` knowledge entry, re-evaluate next run
- **≤ 1 signal** → skip

### 5. How are admin proposals materialised?

**Decision: a new `admin_proposals` SQL table + two native tools + reuse notifications.**

Why a SQL table (not knowledge store):

- Proposals have a lifecycle (`open → triaged → accepted/rejected → implemented`).
  Knowledge store is append-only and a poor fit for mutable state.
- Proposals need structured querying (list by status, filter by category). SQL
  is the right substrate.
- The `approvals` table already proves this pattern works.

Schema:

```sql
CREATE TABLE IF NOT EXISTS admin_proposals (
    proposal_id     TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    category        TEXT NOT NULL,  -- capability | tool | protocol | ux | agent
    evidence_json   TEXT NOT NULL,  -- cross-session pattern evidence (structured JSON)
    remediation     TEXT NOT NULL,  -- suggested fix
    blast_radius    TEXT NOT NULL,  -- low | medium | high
    priority        TEXT DEFAULT 'medium',  -- low | medium | high | critical
    created_by      TEXT NOT NULL,  -- agent_id that created it
    created_at      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'open',  -- open | triaged | accepted | rejected | implemented
    triaged_by      TEXT,
    triaged_at      TEXT,
    decision_reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_admin_proposals_status ON admin_proposals(status);
CREATE INDEX IF NOT EXISTS idx_admin_proposals_category ON admin_proposals(category);
CREATE INDEX IF NOT EXISTS idx_admin_proposals_created_at ON admin_proposals(created_at);
```

Notification: on insert, `admin_proposal_create` writes a row to the existing
`notifications` table with `type='admin_proposal'` and `payload={proposal_id,
title, category, priority}`. CLI/chat TUI already renders notifications.

Gating: reuse `ApprovalQueue { patterns: ["admin.proposal.*"] }` rather than
adding a new capability. Proposal creation is conceptually approval-queue-like.

### 6. Blast radius and safety

**Decision: three-tier safety.**

| Tier | Scope | Gate |
|------|-------|------|
| 1 — ungated | Reading sessions/traces/digests, writing curated learnings to knowledge store | None. Pure reads + knowledge_store writes. |
| 2 — existing gates | Agent evolution via steward → factory → evaluator + auditor + builder | Already enforced: `promotion.rs` allows only evaluator/auditor to call `promotion_record`; strict-mode `agent_revision_create_from_intent` requires both records |
| 3 — admin sign-off | Any revision that changes `NetworkAccess`/`CodeExecution`/`AgentSpawn` capabilities | Approval request at `approval_level: Admin` via existing gateway config escalation rules |

Additional safety:

- **Rate limit**: orchestrator runs at most once per cron period (4h default).
- **Evolution cap**: orchestrator SKILL.md enforces `max 2 agents evolved per run`.
- **Foundational exclusion list**: orchestrator reads an `evolution.exempt_agents`
  knowledge entry (defaulting to `["planner.default", "coder.default",
  "evaluator.default", "auditor.default", "specialized_builder.default",
  "agent-factory.default", "evolution-orchestrator.default",
  "memory-curator.default", "evolution-steward.default"]`) and skips any agent
  in it.
- **Rollback always available**: `agent_revision_rollback` is the emergency exit
  if a bad promotion ships.
- **All actions logged**: `causal_events` + `execution_traces` give a full audit trail.

### 7. New code vs. new agent bundles

**Decision: 3 agent bundles, 2 native tools, 1 SQL table, 0 new capability variants.**

| Component | Type | Count |
|-----------|------|-------|
| Agent bundles (SKILL.md + instructions) | Agent | 3 |
| SQL migration | Rust | 1 |
| Gateway-store module | Rust | 1 (`admin_proposals.rs`) |
| Native tools | Rust | 2 (`admin_proposal_create`, `admin_proposal_list`) |
| New capabilities | — | 0 (reuse `ApprovalQueue` + `ReadAccess`) |

---

## New agent bundles

### `agents/evolution/evolution-orchestrator.default/`

**Role**: Cron-driven root orchestrator of the evolution pipeline.

**Capabilities**:

```yaml
capabilities:
  - type: ReadAccess
    scopes: ["*"]
  - type: WriteAccess
    scopes: ["self.*", "evolution.*"]
  - type: AgentSpawn
    max_children: 10
  - type: SchedulerAccess
    patterns: ["*"]
  - type: SandboxFunctions
    allowed:
      - "knowledge."
      - "execution."
      - "observability."
      - "session."
      - "digest."
      - "admin."
  - type: ApprovalQueue
    patterns: ["admin.proposal.*"]
  - type: BackgroundReevaluation
    min_interval_secs: 14400
    allow_reasoning: true
```

Note: no `AgentRevision`. All revision work is delegated.

**Instructions outline**:

1. On wake, read `knowledge_recall(id="evolution.high_water_mark")`. If absent,
   initialise to `now - 4h`.
2. If no cron job exists for self, call `scheduler_cron_create(message="Run evolution analysis", schedule_expr="0 */4 * * *")`.
3. Call `session_search(status="completed", since=<bookmark>, limit=<max_sessions>)`.
4. If no sessions → update bookmark, end turn.
5. Spawn `memory-curator.default` with `{ session_ids, max_sessions: 50 }`. Wait
   for result.
6. From curator result:
   - For each agent in `agent_scores` where `evolution_recommended=true` AND not
     in exemption list AND under per-run cap: spawn `evolution-steward.default`
     with `{ agent_id, evidence }`. Track per-agent outcome.
   - For each `systemic_gap`: call `admin_proposal_create` with structured
     fields.
7. Update `knowledge_store(id="evolution.high_water_mark", content={ last_processed_at: now, last_run_id, generation: +1 }, visibility: global)`.
8. End turn with a summary: sessions analysed, agents evolved, proposals created.

**Delegation contract**:

- To curator: `{ session_ids: [...], max_sessions: N }` → returns `{ agent_scores: {...}, systemic_gaps: [...], learnings_stored: N }`.
- To steward: `{ agent_id, evidence }` → returns `{ evolved: bool, reason: string, new_revision_id?: string }`.

### `agents/evolution/memory-curator.default/`

**Role**: Cross-session learning distillation + per-agent performance scoring.

**Capabilities**:

```yaml
capabilities:
  - type: ReadAccess
    scopes: ["*"]
  - type: WriteAccess
    scopes: ["self.*", "evolution.*"]
  - type: SandboxFunctions
    allowed:
      - "knowledge."
      - "execution."
      - "observability."
      - "digest."
```

No `AgentSpawn` — leaf agent.

**Instructions outline**:

1. Receive `{ session_ids, max_sessions }` from orchestrator.
2. Cap `session_ids` to `max_sessions`.
3. For each session:
   - `digest_query(session_id)` — read narrative digest
   - `execution_search(session_id, limit=200)` — raw tool traces
   - `observability_search` + `observability_read` — published session report
4. Extract durable learnings:
   - Effective patterns (what worked) → `knowledge_store(tags=["source:memory_curator", "type:effective_pattern", "agent:<id>"])`
   - Error patterns (what repeatedly failed) → `knowledge_store(tags=["source:memory_curator", "type:error_pattern", "agent:<id>"])`
   - Approach improvements (alternative strategies) → `knowledge_store(tags=["source:memory_curator", "type:approach_improvement"])`
5. Compute per-agent metrics from traces/approvals/evals/escalations using the
   multi-signal scoring matrix (section "Selection criterion" above).
6. Identify systemic gaps: errors that recur across multiple agents (not agent-
   specific), missing tools/capabilities mentioned in escalation reasons,
   protocol issues (e.g., agents frequently misusing a specific tool).
7. Return structured JSON:

```json
{
  "agent_scores": {
    "<agent_id>": {
      "failure_rate": 0.42,
      "repeated_errors": ["timeout", "permission_denied"],
      "approval_denial_count": 3,
      "eval_score": 0.41,
      "escalation_count": 2,
      "signals_triggered": 4,
      "evolution_recommended": true,
      "evidence_summary": "<human-readable paragraph>"
    }
  },
  "systemic_gaps": [
    {
      "title": "Agents lack structured HTTP response parsing tool",
      "category": "tool",
      "evidence": "<cross-session pattern>",
      "remediation": "<suggested fix>",
      "blast_radius": "medium",
      "priority": "high"
    }
  ],
  "learnings_stored": 27
}
```

### `agents/evolution/evolution-steward.default/`

**Role**: Per-agent evolve/skip decision + triggers factory pipeline.

**Capabilities**:

```yaml
capabilities:
  - type: ReadAccess
    scopes: ["*"]
  - type: WriteAccess
    scopes: ["self.*"]
  - type: AgentSpawn
    max_children: 5
  - type: SandboxFunctions
    allowed:
      - "knowledge."
      - "agent."
      - "observability."
```

No `AgentRevision`. Evolution goes through `agent-factory.default`.

**Instructions outline**:

1. Receive `{ agent_id, evidence }` from orchestrator.
2. Call `agent_revision_inspect(agent_id)` — current SKILL.md, capabilities,
   script entry, runtime.lock.
3. Call `knowledge_search_by_tags(tags=["source:memory_curator", "agent:<id>"])`
   for historical context (beyond this run's window).
4. Classify root cause:
   - **Instructions-level** (reasoning errors, wrong tool selection, format
     drift) → generate improved SKILL.md body; spawn `agent-factory.default`
     with `{ mode: "improve_instructions", base_agent_id, new_body }`.
   - **Code-level** (script bugs, missing error handling, wrong dependencies) →
     spawn `agent-factory.default` with `{ mode: "improve_script",
     base_agent_id, intent }`.
   - **Systemic** (missing tool/capability — fix isn't in this agent) → return
     `{ evolved: false, reason: "systemic_gap", proposal: {...} }`. Orchestrator
     will forward the proposal to `admin_proposal_create`.
5. If evolution triggered: wait for factory result. On success → return
   `{ evolved: true, new_revision_id }`. On failure → return `{ evolved: false,
   reason: "factory_gate_failed", details }`.

**Delegation contract to factory**: follows the factory's existing input schema
(see `agents/evolution/agent-factory.default/SKILL.md`).

---

## New gateway-side primitives

### 1. `admin_proposals` table (migration v11)

File: `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs`

Add a new migration step with the schema above. Follow the existing migration
pattern — `schema_migrations` table already drives versioning.

### 2. `admin_proposals.rs` gateway-store module

File: `autonoetic-gateway/src/scheduler/gateway_store/admin_proposals.rs`

Public API (follows existing store-module conventions):

```rust
pub struct AdminProposal {
    pub proposal_id: String,
    pub title: String,
    pub category: String,
    pub evidence_json: serde_json::Value,
    pub remediation: String,
    pub blast_radius: String,
    pub priority: String,
    pub created_by: String,
    pub created_at: String,
    pub status: String,
    pub triaged_by: Option<String>,
    pub triaged_at: Option<String>,
    pub decision_reason: Option<String>,
}

pub fn insert_admin_proposal(conn: &Connection, p: &AdminProposal) -> Result<()>;
pub fn list_admin_proposals(
    conn: &Connection,
    status_filter: Option<&str>,
    category_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<AdminProposal>>;
pub fn update_admin_proposal_status(
    conn: &Connection,
    proposal_id: &str,
    status: &str,
    triaged_by: &str,
    decision_reason: Option<&str>,
) -> Result<bool>;
pub fn get_admin_proposal(conn: &Connection, proposal_id: &str) -> Result<Option<AdminProposal>>;
```

Register the module in `autonoetic-gateway/src/scheduler/gateway_store/mod.rs`
and add thin pass-through methods on `GatewayStore` (same pattern as
`insert_agent_revision_transactional`).

### 3. `admin_proposal_create` native tool

File: `autonoetic-gateway/src/runtime/tools/admin_proposal.rs`

Follows the existing `NativeTool` pattern:

```rust
pub struct AdminProposalCreateTool;

impl NativeTool for AdminProposalCreateTool {
    fn name(&self) -> &'static str { "admin_proposal_create" }

    fn is_available(&self, manifest: &AgentManifest) -> bool {
        manifest.capabilities.iter().any(|c|
            matches!(c, Capability::ApprovalQueue { patterns }
                if patterns.iter().any(|p| p == "*" || p.starts_with("admin.proposal")))
        )
    }

    fn execute(&self, ..., arguments_json: &str, ...) -> anyhow::Result<String> {
        // 1. Parse & validate args
        // 2. Mint proposal_id
        // 3. Dedupe check: list open proposals with similar (title, category)
        //    via simple LIKE match — if found, update existing evidence_json
        //    with append & return existing proposal_id
        // 4. Insert admin_proposal row
        // 5. Insert notifications row with type="admin_proposal"
        // 6. Return { ok: true, proposal_id, deduped: bool }
    }
}
```

Tool definition:

```json
{
  "name": "admin_proposal_create",
  "description": "Create a feature-evolution proposal for admin review.",
  "input_schema": {
    "type": "object",
    "properties": {
      "title":        { "type": "string", "maxLength": 200 },
      "category":     { "type": "string", "enum": ["capability", "tool", "protocol", "ux", "agent"] },
      "evidence":     { "type": "object", "description": "Cross-session pattern evidence (structured)" },
      "remediation":  { "type": "string", "description": "Suggested fix" },
      "blast_radius": { "type": "string", "enum": ["low", "medium", "high"] },
      "priority":     { "type": "string", "enum": ["low", "medium", "high", "critical"] }
    },
    "required": ["title", "category", "evidence", "remediation", "blast_radius"],
    "additionalProperties": false
  }
}
```

### 4. `admin_proposal_list` native tool

Same file. Returns proposals filtered by `status`/`category`. Gated by
`ReadAccess` (wildcard or `"admin.*"` scope).

### 5. Register tools

In `autonoetic-gateway/src/runtime/tools/mod.rs`:

```rust
pub mod admin_proposal;
// ...
crate::runtime::tools::admin_proposal::register_tools(&mut registry);
```

---

## Critical files to touch

### New files

| Path | Purpose |
|------|---------|
| `agents/evolution/evolution-orchestrator.default/SKILL.md` | Root orchestrator bundle |
| `agents/evolution/memory-curator.default/SKILL.md` | Session-digestion + scoring bundle |
| `agents/evolution/evolution-steward.default/SKILL.md` | Per-agent evolve-decision bundle |
| `autonoetic-gateway/src/scheduler/gateway_store/admin_proposals.rs` | SQL CRUD module |
| `autonoetic-gateway/src/runtime/tools/admin_proposal.rs` | `admin_proposal_create` + `admin_proposal_list` tools |

### Modified files

| Path | Change |
|------|--------|
| `autonoetic-gateway/src/scheduler/gateway_store/migrate.rs` | Add migration v11 for `admin_proposals` table |
| `autonoetic-gateway/src/scheduler/gateway_store/mod.rs` | Register `admin_proposals` module + pass-through methods |
| `autonoetic-gateway/src/runtime/tools/mod.rs` | Register `admin_proposal` module |
| `docs/AGENTS.md` | Document evolution-orchestrator/curator/steward roles + new tools |
| `config/config-template.yaml` | (Optional) `evolution_pipeline` section for tunable thresholds + exemption list |

### Reference files (read-only during implementation)

| Path | Why |
|------|-----|
| `autonoetic-gateway/src/runtime/post_session_digest.rs` | Pattern for deterministic memory-id hashing |
| `autonoetic-gateway/src/scheduler/gateway_store/observability.rs` | `search_session_transcripts` / `search_execution_traces` — the query surface the curator depends on |
| `autonoetic-gateway/src/scheduler.rs` | `process_due_scheduled_jobs` — how cron fires |
| `autonoetic-gateway/src/runtime/tools/promotion.rs` | Evaluator/auditor gating — do not touch, just understand |
| `agents/evolution/agent-factory.default/SKILL.md` | Factory input schema the steward delegates to |

---

## Phased rollout

### Phase 1 — MVP: read-only analysis + admin proposals

**Ships:**

- `evolution-orchestrator.default` (curator-only delegation, no steward calls)
- `memory-curator.default`
- `admin_proposals` table + migration v11
- `admin_proposal_create` + `admin_proposal_list` tools
- Cron job (every 4h) pointing at the orchestrator
- Bookmark in knowledge store

**Behaviour:**

- On cron tick: analyse sessions, distill learnings into knowledge store, score
  agents, write proposals for systemic gaps.
- Does **not** create any revisions.
- Does **not** touch any existing agent.

**Validation gates before moving to Phase 2:**

- Manually run the pipeline, inspect proposals — are they reasonable?
- Check knowledge store for distilled learnings — are they useful?
- Verify bookmark advances correctly across runs.
- Verify crash safety: kill the orchestrator mid-run, confirm next run re-processes
  the same window without duplicating knowledge entries.

### Phase 2 — autonomous evolution behind existing gates

**Ships:**

- `evolution-steward.default`
- Orchestrator updated to delegate flagged agents to steward
- Integration test: full loop against a deliberately-broken test agent

**Behaviour:**

- Flagged agents (signals ≥ 3) are routed through steward → factory.
- All revisions still pass through evaluator + auditor gates.
- Capability-escalating changes trigger admin approval.
- Exemption list excludes foundational agents.
- Orchestrator caps evolutions at 2 per run.

**Validation gates before moving to Phase 3:**

- Evolve a test agent end-to-end; confirm evaluator/auditor gates fire.
- Verify rollback works when an evolved agent regresses.
- Confirm exemption list is honoured.

### Phase 3 — closed-loop feedback

**Ships:**

- Post-evolution monitoring: track whether evolved agents perform better in the
  next window.
- Auto-rollback on regression (same multi-signal scoring applied to the new
  revision vs. the previous one).
- Proposal lifecycle management: triage queries, acceptance path, impl-tracking.
- Admin-agent bundle: a reasoning agent with `admin_proposal_list` +
  `admin.proposal.triage` that can resolve proposals autonomously under human
  oversight.

---

## Risks and open questions

Items flagged for user input **before implementation starts**:

1. **LLM cost budget**: each pipeline run spawns ~5 agents (orchestrator +
   curator + possibly steward + factory + evaluator + auditor + builder). At 4h
   cadence this is manageable but scales with session volume. **Question**:
   what's the acceptable cost budget per run? Should the orchestrator enforce a
   hard max-sessions-per-batch (suggested default: 50)?

2. **Evolution model: improve vs. recreate**: `agent-factory.default` is built
   to create new agents from scratch. The steward needs to frame its requests
   as "create a replacement agent with these improvements" rather than "edit
   this agent". **Question**: is the create-from-scratch flow acceptable, or do
   we want a new `agent.revision.improve` path that diffs against the current
   SKILL.md and preserves revision lineage?

3. **Foundational agent exclusion**: the MVP excludes `planner.default`,
   `coder.default`, etc. from autonomous evolution. **Question**: is this list
   correct? Should it be configurable in `config.yaml` rather than hard-coded in
   the orchestrator's SKILL.md?

4. **Proposal deduplication**: the same systemic gap may surface across many
   runs. Simple LIKE match on `(title, category)` is proposed. **Question**: is
   this sufficient, or do we need semantic similarity (e.g., embedding-based
   dedup)? Probably LIKE is fine for MVP.

5. **Session volume**: if there are thousands of completed sessions per window,
   analysis cost grows. Proposed max-batch cap of 50 sessions. **Question**:
   acceptable? Or should we sample rather than cap?

6. **Overlapping runs**: 4h cron should prevent overlap, but a very long run
   could theoretically overlap with the next tick. The existing
   `claim_and_advance_due_job` prevents the same cron job from double-firing.
   Mitigation implicit.

7. **Testing strategy**: end-to-end testing requires a working gateway + LLM
   access. Unit tests cover: proposal table CRUD, bookmark logic, scoring
   heuristic against mocked execution traces. Integration tests cover the full
   pipeline against a deliberately-broken test agent in Phase 2.

---

## Not in scope

- Replacing `post_session_digest` — the pipeline **consumes** digests, doesn't
  replace them.
- A proposal-review UI — CLI + notifications are enough for MVP.
- Federation across gateway nodes — each gateway runs its own pipeline.
- Evolving non-agent artifacts (tools, capabilities, gateway code itself). Those
  surface as admin proposals but are not autonomously implemented.

---

## Summary of changes at a glance

**Code size estimate:**

- 3 SKILL.md bundles (~300-500 lines each)
- 1 SQL migration (~20 lines)
- 1 gateway-store module (~150 lines)
- 1 tools module with 2 tools (~250 lines)
- Tool registration + pass-through methods (~20 lines)

**Zero new capabilities. Zero breaking changes to existing agents. Zero changes
to existing evaluation/auditor/promotion logic.**
