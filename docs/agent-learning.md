# Agent Learning

Autonoetic agents can learn from past sessions using three query tools that access the unified gateway database.

## Overview

Every session produces structured data that agents can query later:

| Data | Tool | Table |
|------|------|-------|
| Code execution results | `execution.search` | `execution_traces` |
| Tagged memories/lessons | `knowledge.search_by_tags` | `memories` |
| Session narratives | `digest.query` | Content store (digest.md) |

This enables patterns like:
- "Have I seen this compilation error before?"
- "What lessons did I learn about HTTP clients?"
- "What approaches worked for similar tasks?"

---

## execution.search

Query past code executions to find patterns, errors, and successful commands.

### Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `tool_name` | string | Filter by tool (e.g., `sandbox.exec`) |
| `success` | boolean | Filter by success/failure |
| `error_type` | string | Error classification: compilation, runtime, permission, timeout |
| `command_pattern` | string | SQL LIKE pattern for command |
| `agent_id` | string | Filter by agent |
| `limit` | number | Max results (default: 10) |

### Example: Find Compilation Errors

```json
{
  "tool_name": "sandbox.exec",
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
  "tool_name": "sandbox.exec",
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

## knowledge.search_by_tags

Search tagged memories for lessons, decisions, and facts.

### Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `scope` | string | Memory scope (agent, session, global) |
| `tags` | [string] | Required tags (AND logic) |
| `text` | string | Optional text search in content |
| `limit` | number | Max results (default: 10) |

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
  "tags": ["type:decision"],
  "text": "retry",
  "limit": 5
}
```

---

## digest.query

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
knowledge.search_by_tags({
  "tags": ["type:error_lesson", "domain:http"],
  "limit": 5
})

// 2. Check for past approaches
knowledge.search_by_tags({
  "tags": ["type:approach"],
  "text": "http client",
  "limit": 5
})

// 3. Check execution history
execution.search({
  "command_pattern": "%http%",
  "success": false,
  "limit": 5
})
```

### Pattern 2: Error Recovery

When encountering an error, search for similar past errors:

```json
// After a compilation error
execution.search({
  "error_type": "compilation",
  "success": false,
  "command_pattern": "%<current_file>%",
  "limit": 3
})

// Check if this error was seen before
knowledge.search_by_tags({
  "tags": ["type:error_lesson"],
  "text": "<error_message_snippet>",
  "limit": 3
})
```

### Pattern 3: Decision Context

Before making a significant decision, review past decisions:

```json
knowledge.search_by_tags({
  "tags": ["type:decision"],
  "text": "<relevant_keyword>",
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

## Retention

- **execution_traces**: Default 30 days (configurable via `retention.execution_traces_days`)
- **causal_events**: Default 90 days (configurable via `retention.causal_events_days`)
- **memories**: No automatic expiration (manual management)

Set to `0` to disable pruning.
