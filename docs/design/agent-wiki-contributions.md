# Agent Wiki Contributions

> Tracking issue: [#425](https://github.com/mandubian/autonoetic/issues/425)
> Operator UX: [#426](https://github.com/mandubian/autonoetic/issues/426)
> Quality governance: [#427](https://github.com/mandubian/autonoetic/issues/427)
> Status: Draft design — not yet implemented

## Problem Statement

The wiki system (`wiki_list` / `wiki_get`) provides a curated, read-only documentation corpus bootstrapped from `docs/wiki/` into `runtime/wiki/` at startup. This gives every agent access to platform knowledge — SDK reference, architecture overview, tool guide, approval system, promotion lifecycle, etc.

But the corpus is **only authored at build time** by developers. Agents that discover patterns, write runbooks, or develop reusable guidance during a session have no way to contribute that knowledge back to the wiki. The existing `knowledge_store` tool provides durable fact storage, but those facts are:
- **Session-scoped** by default — they don't survive across root sessions unless explicitly tagged `global`
- **Not curated** — no review, no editorial quality bar
- **Not discoverable by other agents** — no catalog, no structured navigation

A wiki contribution pipeline would let agents **reify knowledge** — promote session-level discoveries into curated, discoverable wiki pages that benefit all future agents and sessions.

---

## Design

### Overview: Reuse the Existing Gate Pipeline

Wiki proposals are **not** a new approval mechanism. They are a new `GateKind` variant that flows through the existing `GateService` pipeline — deduplication, session grants, approval flood cap, operator resolution, timeline events. Everything works the same way as `sandbox_exec` approvals or `user_ask` interactions.

```
Agent → wiki_propose(id, title, content, tags) → GateService.check()
                                                     │
                          ┌──────────────────────────┼──────────────────────┐
                          ▼                          ▼                      ▼
                     PolicyAllowed             AlreadyPending            Suspended
                  (grant covers it)         (same id pending)       (new gate created)
                                                                          │
                                                         Operator → approvals.approve
                                                                  → approvals.reject
                                                                          │
                                                          ┌───────────────┴───────────────┐
                                                          ▼                               ▼
                                                     Promoted                          Rejected
                                               (materialize .md +                (gate dismissed,
                                                update index.toml,             content discarded)
                                                emit causal event)
```

### Single New Tool: `wiki_propose`

This is the **only new tool**. No `wiki_promote`, `wiki_reject`, `wiki_withdraw`, or `wiki_list_proposals` — those are all covered by the existing gate infrastructure and operator CLI.

```
wiki_propose(id, title, content, tags?)
```

**Parameters:**

| Parameter | Required | Description |
|-----------|----------|-------------|
| `id` | Yes | Page ID. Must match `[a-z0-9]+(-[a-z0-9]+)*`. If the ID matches an existing page, this is an **edit**. |
| `title` | Yes | Human-readable title |
| `content` | Yes | Full markdown content (non-empty, ≤ 64 KiB) |
| `tags` | No | Tags for categorization and discovery |

**Returns immediately** — the tool does not suspend the agent. It creates a gate and returns the gate reference:

```json
{
  "ok": true,
  "id": "runbook-agent-creation",
  "gate_id": "gate-abc123",
  "is_edit": false,
  "status": "pending",
  "proposed_at": "2026-06-08T12:00:00Z"
}
```

**Edition**: `wiki_propose` with an existing page ID is an edit. The gate description notes "edit proposal" instead of "new proposal". On promote, the existing `.md` file is overwritten and the `index.toml` entry is updated (title/tags may change). No separate version tracking — last promoted content wins.

**Deduplication**: Proposing the same `id` while a gate is already pending returns `AlreadyPending` — the existing gate ID is reused. The agent can see its proposal is still awaiting review.

### Capability: `WikiContribute`

```yaml
capabilities:
  - type: "WikiContribute"
```

Writing durable documentation is a trust boundary — not every agent should propose wiki pages. The capability is declared in SKILL.md. Agents without it can still read the wiki (always available, Core tier).

Natural candidates: planner, evolution steward, governance author.

### GateKind: `WikiProposal`

A new variant on the existing `GateKind` enum:

```rust
GateKind::WikiProposal {
    page_id: String,
    title: String,
    content: String,
    content_sha256: String,  // for audit, not content-addressing
    tags: Vec<String>,
    is_edit: bool,
    proposed_by_agent: String,
    proposed_by_session: String,
}
```

The `content_sha256` is computed by the gateway and stored in the gate payload. It is NOT used for content-addressing — wiki pages are files, not immutable artifacts. The hash is purely for audit: the `wiki.promoted` causal event records the hash so changes are traceable.

### Gate Resolution: Materialization

When the operator approves the gate (via existing `approvals.approve`), the resolution handler materializes the page:

1. Write `{id}.md` to `runtime/wiki/` (atomic: write-to-temp → rename)
2. Update `index.toml`: add new entry or update existing entry for the ID
3. Emit `wiki.promoted` causal event with `content_sha256`
4. Reload the wiki index in-memory so `wiki_list` / `wiki_get` see the new page immediately

When the operator rejects the gate (via `approvals.reject`):

1. Emit `wiki.rejected` causal event
2. Gate is dismissed, content is discarded

### Causal Events

| Event | Trigger | Payload |
|-------|---------|---------|
| `wiki.proposed` | `wiki_propose` creates a gate | `page_id`, `title`, `content_sha256`, `is_edit`, `proposed_by_agent` |
| `wiki.promoted` | Operator approves | `page_id`, `title`, `content_sha256`, `is_edit`, `approved_by` |
| `wiki.rejected` | Operator rejects | `page_id`, `title`, `rejected_by`, `reason` |

These appear on the session timeline and in the Room TUI alongside other gate events.

### Storage

Wiki proposals live in the existing gate store — no new SQLite table. The gate payload carries all proposal metadata (`page_id`, `title`, `content`, `tags`, `content_sha256`, `is_edit`, `proposed_by_agent`). When the gate is resolved (approved/rejected/cancelled), the gate row is updated with the decision — same lifecycle as every other gate kind.

### Bootstrapping

The bootstrap snapshot (`bootstrap_wiki_snapshot()`) copies `docs/wiki/` into `runtime/wiki/` at startup — but only for pages that don't already exist in `runtime/wiki/`. Promoted pages survive restarts because the materialized files are in the live directory, not the source tree.

The authoritative wiki directory is `runtime/wiki/`. The source tree `docs/wiki/` is a seed, not an override.

---

## Non-Goals

1. **Agent-as-decider.** Operator always ratifies. Same pattern as every other gate kind.

2. **Page deletion.** Once promoted and materialized, pages are permanently available. Deletion is CLI-only, not agent-facing.

3. **Wiki search.** `wiki_list` + tag-based navigation is sufficient. Semantic search can layer on top later.

4. **Content-addressed storage.** Wiki pages are mutable files, not immutable artifacts. A SHA-256 hash in the causal event provides auditability without the complexity of content-addressed storage.

5. **Constitutional rule.** Wiki pages are advisory documentation — they don't control agent behavior. Constitutional rules are mechanically enforced regardless of what wiki pages say. No constitutional surface to govern.

6. **Version history.** Each promote overwrites the `.md` file. The causal event chain is the audit trail (who proposed what, when, with which hash).

---

## Relation to Existing Systems

| System | Purpose | Wiki Contribution |
|--------|---------|-------------------|
| `knowledge_store` | Durable facts, session/private/global scope | Wiki = curated, discoverable, gated documentation |
| `GateService` / `approvals.approve` | Operator approval pipeline | Wiki proposals use the same gate infrastructure |
| `skill_install` | Install remote SKILL.md as new agent | Wiki = knowledge install, not agent install |
| `constitution_propose_amendment` | Propose rule changes | Different mechanism (constitution has its own pipeline). Wiki = simpler, gate-based. |

---

## Implementation Phases

### Phase 1: Core Pipeline

- `WikiContribute` capability in `capability.rs`
- `GateKind::WikiProposal` variant
- `wiki_propose` NativeTool (gated on `WikiContribute`)
- Materialization handler in gate resolution (`on_approval_resolved`)
- Causal events: `wiki.proposed`, `wiki.promoted`, `wiki.rejected`
- Policy enforcement: `WikiContribute` gating on propose

### Phase 2: Operator UX

- Room UI: wiki proposals appear as gates alongside approvals and interactions
- Room keybindings for approve/reject on wiki gates (same y/n as other gates)
- Gateway CLI: `gateway approvals approve <gate-id>` / `reject <gate-id>` (existing commands)

### Phase 3: Quality Governance

- Content quality heuristics on propose (warn, don't block)
- Duplicate detection: similarity scoring against existing pages
- Gate auto-expiry for wiki proposals (reuse existing gate timeout infrastructure)

---

## Risks

1. **Content quality.** Agent-generated docs may be inaccurate. The operator review gate mitigates this — nothing enters the wiki without human approval.

2. **Index bloat.** A large wiki makes `wiki_list` less useful. Tags mitigate this — agents discover by tag, not by scanning.

3. **Materialization race.** Two promotions writing to the same file. Mitigated by gate dedup (same `page_id` reuses existing gate) and atomic file writes.

4. **Index.toml corruption.** Concurrent promotions updating the TOML file. Mitigated by gate dedup (only one pending gate per `page_id`) and atomic write-to-temp → rename.
