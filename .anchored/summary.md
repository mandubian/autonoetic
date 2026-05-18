## Goal

- Implement Phase 4 of the LLM Context Overflow Mitigation Plan: SKILL.md split marker convention (core vs. extended instructions).

## Constraints & Preferences

- Extended instructions are everything after `<!-- extended -->` in SKILL.md body.
- Core instructions are always injected in the system prompt; extended instructions are available for on-demand retrieval via `content_read("extended_instructions")`.
- Old SKILL.md files without the marker work unchanged (backward compatible).
- Feature flag env vars: `AUTONOETIC_STRICT_CONTEXT_GOVERNOR`, `AUTONOETIC_OVERFLOW_RETRY_CLASSIFIER`.

## Progress

### Done

- **Phase 0 + 1 shipped and merged** (PR #216): Safe defaults in `config/config-template.yaml`, startup warning for missing `context_window_tokens`, pluggable `runtime/context_governor/` module with `ReductionStrategy` trait, 4 built-in strategies, feature-gated pipeline in `lifecycle.rs`, provider error classification in all three LLM drivers, static model table in `resolver.rs`.
- **10 review comments on PR #256 all addressed before merge.**
- **Phase 3 shipped and merged** (PR #217): Overflow-aware retry classifier under `AUTONOETIC_OVERFLOW_RETRY_CLASSIFIER=1`.
- **Phase 4 — split marker convention fully implemented:**
  - `split_extended_instructions()` in `runtime/parser.rs:490` — splits SKILL.md body at `<!-- extended -->` marker.
  - `extended_instructions: Option<String>` on `LoadedAgent` in `agent/repository.rs:15`.
  - Split wired into both `load_from_meta()` (line 134) and `load_from_revision_dir()` (line 219) in `repository.rs`.
  - `extended_instructions: Option<String>` field + `with_extended_instructions()` builder on `AgentExecutor` in `lifecycle.rs:184`.
  - All 4 `AgentExecutor::new()` call sites in `execution.rs` pass `loaded.extended_instructions`.
  - Extended instructions written to content store as `"extended_instructions"` (Session visibility) on first session start (`lifecycle.rs:1020-1038`).
  - System prompt auto-injects note: "Extended instructions are available via content_read('extended_instructions')." when extended instructions exist (`lifecycle.rs:719-726`, `lifecycle.rs:1221-1228`).

### In Progress

- None.

### Blocked

- None.

## Key Decisions

- `<!-- extended -->` marker chosen over `## Extended Instructions` to avoid rendering ambiguity in markdown — invisible comment tag that tools like `knowledge_search` can reference.
- `extended_instructions` stored on both `LoadedAgent` (for transport) and `AgentExecutor` (for session lifecycle). The executor owns the session context where content-store writes happen.
- Extended instructions are written to content store with key `extended_instructions` so agents can retrieve them via the existing `content_read` tool — no new tool needed.
- Core instructions omit any note about extended content by default; agents with the marker get an auto-injected note in their system prompt.
- Content store write happens inside `!self.session_started` block (once per session), creating `ContentStore` from `self.gateway_dir`.

## Next Steps

1. Add `<!-- extended -->` marker + extended content to one or more SKILL.md files in `agents/` to exercise the new path.
2. Consider enabling tool schema defaults for planner/factory/builder roles.
3. Revisit the `<!-- extended -->` marker convention documentation if needed.

## Critical Context

- `SKILL.md` body is parsed by `SkillParser::parse()` returning `(AgentManifest, String instructions)`.
- `LoadedAgent.instructions` now holds only core instructions (before `<!-- extended -->`).
- `LoadedAgent.extended_instructions` holds everything after the marker (or `None` if marker absent).
- `AgentExecutor.instructions` is the canonical copy passed to `compose_system_instructions_full()`.
- No changes to the existing knowledge store schema — `content_write` uses the content store at `<gateway_dir>/content/<session_id>/`.
- Currently 828 tests pass, 25 pre-existing failures (unchanged).

## Relevant Files

- `autonoetic-gateway/src/runtime/parser.rs`: `split_extended_instructions()` — split function (line 490).
- `autonoetic-gateway/src/agent/repository.rs`: `LoadedAgent.extended_instructions` (line 15), split wired in `load_from_meta` (line 134) and `load_from_revision_dir` (line 219).
- `autonoetic-gateway/src/runtime/lifecycle.rs`: `AgentExecutor.extended_instructions` field (line 184), `with_extended_instructions()` builder, content store write (lines 1020-1038), system prompt hint injection (lines 719-726, 1221-1228).
- `autonoetic-gateway/src/execution.rs`: All 4 `AgentExecutor::new()` call sites pass `loaded.extended_instructions`.
- `autonoetic-gateway/src/runtime/context.rs`: `compose_system_instructions_full()` — no changes needed (hint injected at call site).
- `config/config-template.yaml`: safe defaults for `prompt_budget` and `context_compression`.
