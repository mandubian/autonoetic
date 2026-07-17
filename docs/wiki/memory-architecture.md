# Memory Architecture

## Two Tiers

The canonical architecture defines two memory tiers:

| Tier | Storage | Lifetime | Visibility | Use Case |
|------|---------|----------|------------|----------|
| **Tier 1** | File-like (content store) | Session / artifact | Configurable | Scratch files, intermediate data, agent artifacts |
| **Tier 2** | Structured knowledge (SQLite + FTS5) | Durable | Configurable | Cross-session facts with provenance |

The agent's current reasoning context is passed in the system prompt and turn-start messages; it is not a separate named tier.

## Tier 1: File-Like Storage

Accessed via content tools and sandbox SDK memory helpers:
- `content_write(name, content, visibility)` — write with visibility control
- `content_patch(name, patch)` — apply a patch to existing content
- `resolve(ref)` — read any artifact/content handle
- `sdk.memory.read(path)` / `sdk.memory.write(path, content)` — script-agent scratch pad

## Tier 2: Structured Knowledge

Accessed via `knowledge_store` / `knowledge_recall` / `knowledge_search` tools and `sdk.memory.remember()` / `sdk.memory.recall()`:
- **Scope**: Logical grouping (e.g., `sdk`, `project`, `agent`)
- **Tags**: AND-matched filtering
- **Confidence**: Optional numeric confidence score
- **Retention**: `stable` (default), `ephemeral`, `1d`, `30d`
- **Visibility**: `session`, `private`, `global`

## Visibility Rules

| Visibility | Who Can Read | When |
|------------|-------------|------|
| `private` | Only the writing agent | Always |
| `session` | All agents in the same root session | During the session |
| `global` | All agents across sessions | Always |

## Cross-Agent Memory Sharing

Facts stored with `session` visibility are readable by all agents in the same root session. This includes:
- A planner reading memories written by sub-agents
- A fibonacci calculator storing results via `sdk.memory.remember`
- Sub-agents reading parent context via `knowledge_search`

Use `private` visibility for data that should only be accessible to the writing agent.
