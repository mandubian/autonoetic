# Context Compression

Autonoetic summarizes old conversation turns when the history approaches the context window limit. This keeps sessions running indefinitely without hitting hard token caps, while preserving recent turns in full and archiving the original context for audit.

## Overview

On every turn, after budget enforcement, the gateway checks whether conversation history exceeds a configurable percentage of the context window. When it does:

1. Old turns are identified as compressible (all except the last N turns)
2. Tool-call groups are kept together — an assistant message with `tool_calls` and its corresponding `Role::Tool` results are never split across the compress/keep boundary
3. A cheap LLM summarizes the compressible portion into a concise summary
4. The original uncompressed history is written to the content store
5. The working history is replaced with: system messages → summary → recent turns

## Configuration

Add a `context_compression` section to your gateway YAML:

```yaml
context_compression:
  # Master switch. Default: false
  enabled: true
  # LLM preset for summarization (should be a cheap/fast model).
  # Mutually exclusive with provider/model below.
  llm_preset: haiku
  # Inline provider/model if not using a preset.
  # provider: anthropic
  # model: claude-3-haiku-20240307
  # Compress when conversation tokens exceed this % of the context window.
  # Default: 60.0
  threshold_pct: 60.0
  # Number of recent turns to always keep in full (not summarized).
  # Default: 3
  recent_turns_to_keep: 3
  # Maximum size of the compressed summary in tokens.
  # Default: 500
  max_summary_tokens: 500
  # Minimum turns between compression operations.
  # Prevents thrashing when token count oscillates around the threshold.
  # Default: 3
  min_turns_between_compression: 3
```

### Per-Agent Overrides

Agents can override compression parameters via their SKILL.md manifest:

```yaml
metadata:
  autonoetic:
    agent:
      id: "my-agent"
    compression:
      threshold_pct: 40.0
      recent_turns_to_keep: 5
      llm_preset: cheap
```

Agent overrides take priority over gateway defaults for `threshold_pct`, `recent_turns_to_keep`, and `llm_preset`. The gateway `enabled` flag is the sole gate — agents cannot enable compression if the gateway hasn't, or disable it if the gateway has.

## How It Works

### Turn Flow

```
Budget enforcement → Context compression → Model routing → LLM call
```

Budget enforcement (trim/demote) runs **before** compression. Recommended: set `context_compression.threshold_pct` slightly below `prompt_budget.warn_at_pct` so compression fires first, preserving information that budget enforcement would discard.

### Split Logic

Messages are split into compressible (old) and kept (recent) ranges:

- **System messages** are always kept, never summarized
- The last N user/assistant exchange pairs are kept in full
- **Tool-call groups** are kept together — if an assistant message with `tool_calls` and its corresponding tool results straddle the split boundary, the boundary adjusts to keep them on the same side
- `[COMPRESSED CONTEXT` messages from previous compressions are skipped during summarization to avoid summary-of-summary

### Compressed History Structure

After compression, the working history is:

```
[system messages]
[COMPRESSED CONTEXT - Turn N]
  Summary of goals, decisions, open items, key facts...
[user message]          ← recent turn
[assistant response]    ← recent turn
...
```

### Checkpoint Integration

`CompressionMetadata` (last compression turn, count, content handle) is stored in session checkpoints. On resume:

- Metadata is restored from checkpoint
- `min_turns_between_compression` prevents re-compression until enough new turns have elapsed
- Previous compression summaries in the history are preserved and not re-summarized

### Content Store

Every compression writes the **original uncompressed** history to the content store with visibility `Private`. The handle is stored in `CompressionMetadata.compressed_context_handle` and logged to the causal chain. This enables:

- **Audit**: inspect what was actually said before compression
- **Restore**: reconstruct the full history if needed
- **Replay**: support for future quality regression testing

## Child Agent Context Handoff

When a parent agent delegates to a child via `agent_spawn`, the parent can include a bounded context summary:

```json
{
  "agent_id": "coder",
  "context": "The user wants a REST API with CRUD endpoints. We decided on SQLite for storage. Key files: src/api.rs, src/db.rs",
  "message": "Implement the PUT /items/:id endpoint"
}
```

The context is injected as a structured `[Context]` section in the child's kickoff message:

```
[Context]
The user wants a REST API with CRUD endpoints...

[Task]
Implement the PUT /items/:id endpoint
```

The parent is responsible for curating what the child needs — the gateway does not automatically share the parent's full conversation history. Schema enforcement includes the `context` field so agents with `io.accepts` schemas can validate it.

## Graceful Degradation

Compression failures never break the session. If any of these occur, the original history is preserved and the turn proceeds normally:

- Compression disabled in config
- Minimum interval not yet elapsed
- Token threshold not exceeded
- Not enough messages to compress
- No compression LLM configured
- Compression LLM driver fails to build
- Compression LLM call fails (network error, rate limit, etc.)
- Compression LLM returns empty/whitespace-only summary
- Content store persist fails (compression still applies, but audit handle is lost)

## Known Limitations

- **Token estimation** uses a ~4 chars/token heuristic. This can overestimate for code-heavy conversations and underestimate for CJK text. Improvements are cross-cutting with the prompt budget feature.
- **Tool call arguments** (structured JSON in `tool_calls[].arguments`) are lost in summarization — only message content text is included in the summary.
- **Content store growth** — every compression writes a full snapshot. No retention or cleanup mechanism exists yet.
- **Compression LLM timeout** — no explicit timeout beyond driver defaults.
- **No quality regression framework** — no automated testing that compressed sessions produce equivalent outcomes to uncompressed sessions.

## Related Code

| Component | Path |
|-----------|------|
| Compression core | `autonoetic-gateway/src/runtime/compression.rs` |
| Lifecycle integration | `autonoetic-gateway/src/runtime/lifecycle.rs` (after budget enforcement, before model routing) |
| Checkpoint storage | `autonoetic-gateway/src/runtime/checkpoint.rs` (`compression_metadata` field) |
| Resume restoration | `autonoetic-gateway/src/execution.rs` (3 restore points) |
| Child context handoff | `autonoetic-gateway/src/runtime/tools/agent.rs` (`SpawnAgentArgs.context`) |
| Gateway config | `autonoetic_types::config::ContextCompressionConfig` |
| Agent manifest override | `autonoetic_types::agent::CompressionConfig` |

## Related Docs

- [Prompt Budget](budget.md) — token budget tracking and enforcement
- [Session budgets](../../reference/budgets.md) — per-session limits and model cost estimation
