# Agent Wiki Contributions

> Tracking issue: [#425](https://github.com/mandubian/autonoetic/issues/425)
> Operator UX: [#426](https://github.com/mandubian/autonoetic/issues/426)
> Quality governance: [#427](https://github.com/mandubian/autonoetic/issues/427)
> Status: Draft design — not yet implemented

## Problem Statement

The wiki system (`wiki.list` / `wiki.get`) provides a curated, read-only documentation corpus bootstrapped from `docs/wiki/` into `.gateway/wiki/` at startup. This gives every agent access to platform knowledge — SDK reference, architecture overview, tool guide, approval system, promotion lifecycle, etc.

But the corpus is **only authored at build time** by developers. Agents that discover patterns, write runbooks, or develop reusable guidance during a session have no way to contribute that knowledge back to the wiki. The existing `knowledge_store` tool provides durable fact storage, but those facts are:
- **Session-scoped** by default — they don't survive across root sessions unless explicitly tagged `global`
- **Not curated** — no review, no editorial quality bar
- **Not discoverable by other agents** — no catalog, no structured navigation

A wiki contribution pipeline would let agents **reify knowledge** — promote session-level discoveries into curated, discoverable wiki pages that benefit all future agents and sessions.

---

## Design

### Overview

```
Agent → wiki.propose(id, title, content, tags) → review queue
        └── Optional: wiki.withdraw(id) removes own pending proposal
Operator/steward → wiki.promote(id)    — accept and publish
                → wiki.reject(id)      — reject with optional reason
                → wiki.list_proposals  — view the review queue
```

### Lifecycle State Machine

```
                 wiki.propose
    (none) ────────────────────► Pending
                                    │
                    ┌───────────────┼───────────────┐
                    ▼               │               ▼
              Promoted          Cancelled       Rejected
              (wiki.get        (withdraw/        (operator
               returns it)     timeout)          rejects)
```

### Capability Model

| Action | Required Capability | Tier |
|--------|-------------------|------|
| `wiki.list` / `wiki.get` | None (always available) | Core |
| `wiki.propose` | `WikiContribute` | Workflow |
| `wiki.withdraw` | Implicit (owner) | Workflow |
| `wiki.list_proposals` | None (always available) | Core |
| `wiki.promote` / `wiki.reject` | Operator-only (CLI / Room) | N/A |

**Why `WikiContribute` is a capability**: Not every agent should be able to propose wiki pages. Writing durable documentation is a trust boundary — it requires judgment about what knowledge is valuable and accurate. The capability is declared in SKILL.md like any other:

```yaml
capabilities:
  - type: "WikiContribute"
```

Agents without `WikiContribute` can still read the wiki (always available). The evolution steward and planner are natural candidates for this capability.

### Tool Interface

#### `wiki.propose`

```json
{
  "tool": "wiki.propose",
  "id": "runbook-agent-creation",
  "title": "Agent Creation Runbook",
  "content": "# Agent Creation Runbook\n\n...",
  "tags": ["runbook", "agent-creation", "workflow"]
}
```

**Parameters:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `id` | Yes | Unique page ID (lowercase, hyphenated, alphanumeric + `-`) |
| `title` | Yes | Human-readable title |
| `content` | Yes | Full markdown content of the page |
| `tags` | No | Tags for categorization and discovery |

**Validation:**
- `id` must match `[a-z0-9]+(-[a-z0-9]+)*` (kebab-case)
- `id` must not collide with any existing wiki page or pending proposal
- `content` must be non-empty and ≤ 64 KiB
- `tags` array, each ≤ 64 chars

**Return:**
```json
{
  "ok": true,
  "id": "runbook-agent-creation",
  "status": "pending",
  "proposed_by": {"agent_id": "planner.default", "session_id": "sess-abc123"},
  "proposed_at": "2026-06-08T12:00:00Z"
}
```

#### `wiki.withdraw`

```json
{
  "tool": "wiki.withdraw",
  "id": "runbook-agent-creation"
}
```

Only the proposing agent (same `agent_id` and `root_session_id`) can withdraw.

#### `wiki.list_proposals`

```json
{
  "tool": "wiki.list_proposals"
}
```

**Return:**
```json
{
  "proposals": [
    {
      "id": "runbook-agent-creation",
      "title": "Agent Creation Runbook",
      "tags": ["runbook", "agent-creation"],
      "status": "pending",
      "proposed_by": {"agent_id": "planner.default"},
      "proposed_at": "2026-06-08T12:00:00Z"
    }
  ]
}
```

#### Operator Actions (CLI / Room)

```
gateway wiki proposals              # list all pending proposals
gateway wiki promote <id>           # accept and publish
gateway wiki reject <id> [reason]   # reject with optional reason
```

Promoted proposals are written to `.gateway/wiki/` as new `.md` files and the `index.toml` is updated. This happens at runtime — no source-tree change needed.

### Storage

Proposals are stored in the gateway store (SQLite) alongside other durable records. They persist across gateway restarts.

**Schema:**
```sql
wiki_proposals (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    tags TEXT,             -- JSON array
    status TEXT NOT NULL,  -- 'pending', 'promoted', 'rejected', 'cancelled'
    proposed_by_agent TEXT,
    proposed_by_session TEXT,
    proposed_at TEXT,      -- RFC 3339
    decided_at TEXT,
    decided_by TEXT,       -- operator identity
    rejection_reason TEXT
)
```

### Bootstrapping

Promoted pages are materialized to `.gateway/wiki/`:
1. A `.md` file is written (using the proposal `id` as filename)
2. An entry is appended to `index.toml`
3. The page is immediately available via `wiki.list` / `wiki.get`

These materialized files survive restarts — the bootstrap snapshot only seeds initial pages; the live `.gateway/wiki/` directory is the authoritative source.

### Timeout

Pending proposals can be set to auto-expire after a configurable duration (default: 7 days). Expired proposals move to `cancelled` status with `rejection_reason: "expired"`.

---

## Non-Goals

1. **Agent-as-decider for wiki proposals.** The operator (human) always ratifies wiki contributions. This mirrors the constitution amendment pattern (`constitution_propose_amendment` → operator ratifies).

2. **Edit/update existing pages.** Proposals are new pages only. Editing existing pages (even agent-authored ones) requires a separate proposal. This avoids versioning complexity for now.

3. **Page deletion.** Once promoted and materialized, pages are permanently available. Deletion is a separate concern (CLI-only, not agent-facing).

4. **Wiki search.** The existing `wiki.list` + index-based navigation is sufficient for ~20-50 pages. Semantic search can layer on top later.

---

## Relation to Existing Systems

| System | Purpose | Wiki Contribution |
|--------|---------|-------------------|
| `knowledge_store` | Durable facts, session/private/global scope | Wiki = curated, discoverable, reviewed facts |
| `constitution_propose_amendment` | Propose rule changes | Same pattern: agent proposes, human ratifies |
| `skill_install` | Install remote SKILL.md as new agent | Wiki = knowledge install, not agent install |
| `observability_search` | Discover published session reports | Wiki = curated guidance, not raw reports |

---

## Implementation Phases

### Phase 1: Core Pipeline (propose → promote/reject)

- `WikiContribute` capability in `capability.rs`
- `wiki_proposal` SQLite table in gateway store
- `wiki.propose`, `wiki.withdraw`, `wiki.list_proposals` NativeTools
- `wiki.promote` / `wiki.reject` RPC + CLI commands
- Materialization to `.gateway/wiki/` on promote
- Policy enforcement: `WikiContribute` gating on propose

### Phase 2: Operator UX

- Room UI: `/wiki proposals` slash command, approve/reject with keybindings
- Gateway CLI: `wiki proposals`, `wiki promote`, `wiki reject`
- Timeline events for proposal lifecycle

### Phase 3: Quality and Governance

- Proposal timeout with configurable TTL
- Quality heuristics: minimum length, required structure
- Duplicate detection: similarity scoring against existing pages
- Constitutional rule for wiki governance (if needed)

---

## Risks

1. **Content quality.** Agent-generated docs may be inaccurate. The operator review gate mitigates this — nothing enters the wiki without human approval.

2. **Index bloat.** A large wiki makes `wiki.list` less useful. Tags and the `wiki.get` pattern mitigate this — agents discover by tag, not by scanning.

3. **Materialization race.** Two promoted proposals writing to the same file simultaneously. Mitigated by the id uniqueness constraint in the proposal store and the file write being atomic (write-to-temp → rename).

4. **Constitution drift.** If wiki pages describe rules that contradict the constitution, agents may be confused. Mitigated by the review gate — operators validate accuracy before promoting.

---

## Open Questions

1. **Should wiki proposals attach to a constitution rule?** e.g., a runbook that explains how to comply with P-2.25. If so, the proposal could reference a rule ID.

2. **Should promoted wiki pages trigger a causal event?** To maintain audit trail. Likely yes — `wiki.promoted` event with the page digest.

3. **Should agents be able to propose edits to existing pages?** Currently scoped to new pages only. Edit proposals could follow the same pipeline with a `base_id` field.

4. **Should wiki pages be content-addressed?** They're written to files currently; content-addressing would make them immutable and auditable, but adds complexity.
