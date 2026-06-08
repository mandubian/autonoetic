# Memory Architecture

## Three Tiers

| Tier | Storage | Lifetime | Visibility | Use Case |
|------|---------|----------|------------|----------|
| **Tier 0** | Session context | Single turn | Agent-only | Working memory for the current reasoning step |
| **Tier 1** | File-like (content store) | Session | Configurable | Scratch files, intermediate data, agent artifacts |
| **Tier 2** | Structured knowledge (SQLite + FTS5) | Durable | Configurable | Cross-session facts with provenance |

## Tier 1: File-Like Storage

Accessed via `sdk.memory.read/write` or `content_write`:
- `sdk.memory.write(path, content)` — private scratch pad
- `sdk.memory.read(path)` — read from Tier 1
- `content_write(name, content, visibility)` — write with visibility control

## Tier 2: Structured Knowledge

Accessed via `knowledge_store/recall/search` tools:
- **Scope**: Logical grouping (e.g., `sdk`, `project`, `agent`)
- **Tags**: AND-matched filtering
- **Confidence**: Optional numeric confidence score
- **Retention**: `stable` (default), `ephemeral`, `1d`, `30d`
- **Visibility**: `session` (default), `private`, `global`

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
