# FTS Session Search

> Full-text search across conversation transcripts, enabling agents to learn from past sessions.

## Overview

Autonoetic automatically persists conversation history at hibernation points and session close, indexes it with SQLite FTS5, and exposes two tools:

- **`session.search`** — FTS5 full-text search with bm25 ranking
- **`session.peek`** — Read the raw transcript of a session (turn counts, role breakdown, truncated excerpt)

## Architecture

```
Session Execution
       │
       ▼
persist_history_to_content_store()
       │
       ├──→ Content Store (SHA-256 blob)
       │         └── .gateway/content/sha256/ab/c123...
       │
       └──→ GatewayStore (SQLite)
                 ├── session_transcripts (structured metadata + excerpt)
                 └── session_transcripts_fts (FTS5 virtual table)
```

### Storage

| Layer | What | How |
|-------|------|-----|
| **Content Store** | Full conversation history (redacted `Vec<Message>` as JSON) | Content-addressed (SHA-256), deduplicated, immutable |
| **session_transcripts** | Metadata + searchable excerpt (max 8KB plaintext) | SQL table with structured columns |
| **session_transcripts_fts** | FTS5 virtual table for full-text search | Content-sync triggers (INSERT/DELETE/UPDATE) |

### Excerpt Extraction

`extract_searchable_excerpt()` converts `Vec<Message>` to bounded plaintext:
- Prefixes each message with role label (`[system]`, `[user]`, `[assistant]`, `[tool]`)
- Skips empty messages
- Caps at 8,000 characters
- Used for both FTS indexing and `session.peek`

## Tools

### session.search

```python
result = sdk.tools.invoke("session.search", {
    "query": "API authentication token",       # FTS5 MATCH syntax
    "agent_id": "researcher.default",           # Filter by agent
    "root_session_id": "sess-abc123",           # Filter by workflow root
    "status": "completed",                      # completed | suspended | failed
    "since": "2026-04-01T00:00:00Z",           # ISO 8601
    "limit": 20                                 # 1-100, default 20
})
# → {
#     "results": [
#       {
#         "session_id": "sess-xyz",
#         "root_session_id": "sess-abc",
#         "agent_id": "researcher.default",
#         "status": "completed",
#         "turn_count": 42,
#         "started_at": "2026-04-02T10:00:00Z",
#         "ended_at": "2026-04-02T10:05:00Z",
#         "excerpt": "[user]: Search for API docs...\n[assistant]: ...",
#         "transcript_handle": "sha256:abc123..."
#       }
#     ],
#     "count": 1
#   }
```

**Ranking:** When `query` is provided, results are ordered by `bm25(session_transcripts_fts)` (SQLite FTS5 bm25 ranking). Without a query, results are ordered by `started_at DESC`.

**ACL:** Agents can only search their own sessions and child sessions of the current root. Cross-agent searches are restricted to the caller's own agent ID.

### session.peek

```python
result = sdk.tools.invoke("session.peek", {
    "transcript_handle": "sha256:abc123...",    # From session.search results
    "max_length": 500                            # 50-5000, default 500
})
# → {
#     "summary": "[user]: Search for API docs...\n[assistant]: Found docs at...",
#     "turn_count": 42,
#     "user_turns": 10,
#     "assistant_turns": 28,
#     "tool_turns": 4,
#     "transcript_handle": "sha256:abc123..."
#   }
```

Reads the full transcript from the content store and returns a truncated text excerpt with turn statistics. No LLM call — deterministic truncation. Accepts either a `transcript_handle` or a `session_id`.

## Schema

### session_transcripts table

| Column | Type | Description |
|--------|------|-------------|
| `transcript_id` | TEXT PRIMARY KEY | Unique ID (prefix: `stx-`) |
| `session_id` | TEXT NOT NULL | Session ID |
| `root_session_id` | TEXT NOT NULL | Root session (workflow) ID |
| `agent_id` | TEXT NOT NULL | Agent that ran the session |
| `revision_id` | TEXT | Pinned agent revision |
| `user_id` | TEXT | User who initiated the session |
| `started_at` | TEXT NOT NULL | ISO 8601 timestamp |
| `ended_at` | TEXT | ISO 8601 timestamp (set on close) |
| `status` | TEXT NOT NULL | `completed`, `suspended`, `failed` |
| `turn_count` | INTEGER NOT NULL | Number of messages |
| `transcript_handle` | TEXT | Content store handle for full history |
| `excerpt` | TEXT | Searchable plaintext excerpt (max 8KB) |
| `origin_node_id` | TEXT | Federation origin node |

### FTS5 virtual table

```sql
CREATE VIRTUAL TABLE session_transcripts_fts USING fts5(
    excerpt,
    content='session_transcripts',
    content_rowid='rowid'
);
```

Content-sync triggers keep the FTS index in sync with the base table on INSERT, DELETE, and UPDATE.

## Lifecycle Integration

History is persisted at two points:

1. **Hibernation yield** (in `execute_with_history`): When a session yields for hibernation, `persist_history_to_content_store()` is called with the current history. This merges with previously persisted history (for session continuity) and bounds to 400 messages.

2. **Session close** (in `close_session`): When a session ends, the final `last_history` (retained from `execute_with_history`) is persisted. This ensures complete capture even for sessions that end without hibernation.

Both paths:
- Write the full redacted history to the content store (SHA-256 blob)
- Extract a searchable excerpt (max 8KB plaintext)
- Upsert a `session_transcripts` record with metadata

## Files

| File | Role |
|------|------|
| `autonoetic-gateway/src/runtime/lifecycle.rs` | `persist_history_to_content_store()`, `extract_searchable_excerpt()`, `close_session()` integration |
| `autonoetic-gateway/src/scheduler/gateway_store.rs` | Schema (session_transcripts, FTS5), `upsert_session_transcript()`, `search_session_transcripts()` |
| `autonoetic-gateway/src/runtime/tools/session.rs` | `SessionSearchTool`, `SessionSummarizeTool`, `enforce_search_acl()` |
| `autonoetic-types/src/causal_chain.rs` | `SessionTranscriptRecord` type |
