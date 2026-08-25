# Agent Learning

> Planned next step: a published-report-first observability URI surface was proposed in `docs/archived/plan-session-observability-uri-surface.md` (archived plan). That plan tightens cross-session access and defines canonical URIs for reports, causal events, traces, and narratives.

Autonoetic agents can learn from past sessions using three query tools that access the unified gateway database.

## Overview

Every session produces structured data that agents can query later:

| Data | Tool | Table |
|------|------|-------|
| Code execution results | `execution_search` | `execution_traces` |
| Tagged memories/lessons | `knowledge_search` | `memories` |
| Session narratives | `digest_query` | Content store (digest.md) |

This enables patterns like:
- "Have I seen this compilation error before?"
- "What lessons did I learn about HTTP clients?"
- "What approaches worked for similar tasks?"

### Storing learnings (`knowledge_store`)

Use **`knowledge_store`** to persist facts and lessons. **`visibility`** defaults to **`session`**: every agent participating in the **same workflow session** can read the row—no separate share tool. Use **`private`** for writer-only facts, **`global`** for cross-session reference material, and **`retention`** (`stable`, `ephemeral`, `1d`, `30d`) for TTL. To widen who can read an existing id, call **`knowledge_store` again** with the same **`id`** and updated **`visibility`** (upsert).

---

## execution_search

Query past code executions to find patterns, errors, and successful commands.

### Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `tool_name` | string | Filter by tool (e.g., `sandbox_exec`) |
| `success` | boolean | Filter by success/failure |
| `error_type` | string | Error classification: compilation, runtime, permission, timeout |
| `command_pattern` | string | SQL LIKE pattern for command |
| `agent_id` | string | Filter by agent |
| `session_id` | string | Narrow to this session and its nested sessions. **Defaults to the caller's root session; may only narrow within it** |
| `limit` | number | Max results (default: 10) |

### Ownership scope (#1062)

`execution_search` reads raw `stdout`/`stderr` out of the trace store, so it is
bounded by the caller's **root session** — the trust domain peers already share
for content visibility (see [content-visibility.md](../internals/storage/content-visibility.md)).

- Omit `session_id` and you search your own root plus everything nested under it.
- Pass a `session_id` inside your root to narrow further (e.g. one child).
- Pass one outside your root and the call is refused — cross-root search is an
  operator privilege, not an agent one.
- The response echoes `session_scope`, so an empty result set reads as "not in
  your root" rather than "no such trace".

Scope is the first gate, the egress label below is the second: scope decides
which traces exist for you at all; the label decides how much of each one your
sink may see. Before #1062 there was no first gate — an omitted `session_id`
searched every session in the store, and `execution_search` is available to
every agent regardless of capabilities.

### Example: Find Compilation Errors

```json
{
  "tool_name": "sandbox_exec",
  "success": false,
  "error_type": "compilation",
  "command_pattern": "%client.rs%",
  "limit": 5
}
```

Returns:
```json
[
  {
    "trace_id": "tr-abc123",
    "command": "cargo build --manifest-path client.rs",
    "exit_code": 1,
    "stderr": "error[E0277]: the trait bound `Future + Send` is not satisfied...",
    "timestamp": "2026-03-15T10:30:00Z",
    "duration_ms": 4500
  }
]
```

### Example: Find Successful Test Commands

```json
{
  "tool_name": "sandbox_exec",
  "success": true,
  "command_pattern": "%pytest%",
  "limit": 10
}
```

### Error Types

| Type | Description |
|------|-------------|
| `compilation` | Syntax errors, type mismatches, missing imports |
| `runtime` | Panics, exceptions, null pointer errors |
| `permission` | File access denied, network blocked |
| `timeout` | Execution exceeded time limit |
| `validation` | Input validation failures |
| `resource` | Out of memory, disk full |

---

## knowledge_search

Search memories for lessons, decisions, and facts within a scope — by content,
by tags, or both.

### Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `scope` | string | **Required.** Knowledge namespace (e.g. `digest.lesson`, `general`), not the same as visibility |
| `query` | string | Optional substring filter on content |
| `tags` | [string] | Optional tags — AND logic (every listed tag must be present) |
| `limit` | number | Max results, 1–100 (default: 10) |

### Tag Conventions

Tags follow a `type:value` convention:

| Tag Pattern | Description |
|-------------|-------------|
| `type:error_lesson` | What went wrong and how to fix |
| `type:decision` | Choices made and rationale |
| `type:approach` | Strategies that worked (or didn't) |
| `type:fact` | Discovered facts about codebase |
| `type:open_item` | Unresolved issues |
| `domain:http` | HTTP/networking related |
| `domain:database` | Database related |
| `domain:auth` | Authentication/authorization |

### Example: Find HTTP Error Lessons

```json
{
  "scope": "agent",
  "tags": ["type:error_lesson", "domain:http"],
  "limit": 10
}
```

Returns:
```json
[
  {
    "memory_id": "mem-xyz789",
    "content": "Async trait methods in this codebase require explicit `+ Send` bound. Add `+ Send` to trait bounds when using async fn in traits.",
    "tags": ["type:error_lesson", "domain:http", "domain:async"],
    "confidence": 0.95,
    "writer_agent_id": "coder.default",
    "created_at": "2026-03-15T10:45:00Z"
  }
]
```

### Example: Find Decisions About Retry Logic

```json
{
  "scope": "decisions",
  "tags": ["type:decision"],
  "query": "retry",
  "limit": 5
}
```

---

## digest_query

Search past session digests for approaches and reasoning.

### Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `session_id` | string | Specific session (optional) |
| `agent_id` | string | Filter by agent (optional) |
| `query` | string | Text search in digest content |
| `limit` | number | Max results (default: 5) |

### Example: Find Sessions That Mentioned "Backoff"

```json
{
  "query": "backoff",
  "limit": 5
}
```

Returns digest excerpts with context:
```json
[
  {
    "session_id": "session-abc",
    "agent_id": "coder.default",
    "turn": 3,
    "excerpt": "Decision: Exponential backoff over fixed delay (user requirement). Lesson: Async trait methods require explicit `+ Send` bound.",
    "timestamp": "2026-03-15T10:30:00Z"
  }
]
```

---

## Learning Patterns

### Pattern 1: Pre-Task Research

Before starting a task, search for related lessons:

```json
// 1. Check for error lessons in this domain
knowledge_search({
  "scope": "lessons",
  "tags": ["type:error_lesson", "domain:http"],
  "limit": 5
})

// 2. Check for past approaches
knowledge_search({
  "scope": "lessons",
  "tags": ["type:approach"],
  "query": "http client",
  "limit": 5
})

// 3. Check execution history
execution_search({
  "command_pattern": "%http%",
  "success": false,
  "limit": 5
})
```

### Pattern 2: Error Recovery

When encountering an error, search for similar past errors:

```json
// After a compilation error
execution_search({
  "error_type": "compilation",
  "success": false,
  "command_pattern": "%<current_file>%",
  "limit": 3
})

// Check if this error was seen before
knowledge_search({
  "scope": "lessons",
  "tags": ["type:error_lesson"],
  "query": "<error_message_snippet>",
  "limit": 3
})
```

### Pattern 3: Decision Context

Before making a significant decision, review past decisions:

```json
knowledge_search({
  "scope": "decisions",
  "tags": ["type:decision"],
  "query": "<relevant_keyword>",
  "limit": 5
})
```

---

## Memory Extraction

The post-session digest agent automatically extracts memories from completed sessions:

1. **Error Lessons**: What went wrong, root cause, fix applied
2. **Decisions**: Choices made and alternatives considered
3. **Approaches**: Strategies that worked (or didn't)
4. **Facts**: Discovered facts about the codebase
5. **Open Items**: Unresolved issues for future sessions

These are tagged and stored in the `memories` table for cross-session retrieval.

---

## Automatic Wake-Time Priming

The tools above are **pull-based** — the agent has to think to call them. The gateway also **pushes** a bounded "Prior Knowledge (from past sessions)" block into the system prompt at every turn, no tool call required, built from the agent's own tagged digest/quality-signal memories.

This priming is **task-matched**, not merely the most recent memories: a candidate pool (deduped, up to 50) is scored against the incoming task text by token-overlap relevance, with error lessons (`digest.error_pattern` / `digest.lesson` scopes) winning ties; unmatched slots are filled by recency so the block is never empty just because nothing scored. Each line carries provenance so the agent can weigh it:

```
Prior Knowledge (from past sessions)

- (error lesson) weather api requires retry on 429 rate limit responses [from session sess-abc1]
- unrelated database schema migration notes [from session sess-def2]
```

How many memories are injected is controlled by `Profile::memory_priming_limit()` (Starter=3, Standard=5, Expert=10). The `auto_learning.task_matched_recall` config flag (default `true`) gates the relevance ranking; set it `false` to fall back to pure recency, matching the block's original behavior before task-matching was added.

Push (this section) and pull (`execution_search`/`knowledge_search`/`digest_query` above) are complementary: priming surfaces what's *likely* relevant without being asked; the query tools let an agent dig further once it has a specific question the priming block didn't answer.

---

## Retention

- **execution_traces**: Default 30 days (configurable via `retention.execution_traces_days`)
- **causal_events**: Default 90 days (configurable via `retention.causal_events_days`)
- **memories**: No automatic expiration (manual management)

Set to `0` to disable pruning.
