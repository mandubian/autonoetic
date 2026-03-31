# Plan: Tools Modularization

**Triggered by:** [plan-agent-revision-evaluation-federation-mvp.md](plan-agent-revision-evaluation-federation-mvp.md) — "Keep gateway tooling modularization as a post-MVP maintenance task. Split `autonoetic-gateway/src/runtime/tools.rs` into smaller topic-focused modules once the MVP tool surface stabilizes."

**Current state:** `autonoetic-gateway/src/runtime/tools.rs` — 8,863 lines, 36 tools + helpers + registry in one file. `tools_promotion.rs` (250 lines, 2 tools) already split out as precedent.

**Goal:** Split into topic-focused, pluggable modules with no behavior or policy changes, so tools can be added/removed independently while preserving current runtime contracts.

---

## 1. Target Structure

```
autonoetic-gateway/src/runtime/
  tools/
    mod.rs                 # NativeTool trait, NativeToolRegistry, default_registry(), shared helpers
    sandbox.rs             # sandbox.exec
    web.rs                 # web.search, web.fetch
    content.rs             # content.write, content.read
    artifact.rs            # artifact.build, artifact.inspect, artifact.resolve_ref
    execution.rs           # execution.search
    knowledge.rs           # knowledge.store/.recall/.search/.search_by_tags/.share, digest.query
    session.rs             # session.escalate
    agent.rs               # agent.spawn, agent.exists, agent.discover
    agent_revision.rs      # agent.revision.{create,list,inspect,promote,rollback,diff}
    eval.rs                # eval.{suite.publish,run,compare,report}
    workflow.rs            # approval.status, workflow.{wait,state,cancel_task}
    user_interaction.rs    # user.ask, user.interaction.status
    digest.rs              # digest.annotate
  tools_promotion.rs       # promotion.record, promotion.query (already split, keep or merge later)
```

## 2. Module Sizes

Line counts measured from the current monolith (struct definition through end of impl + private helpers that sit above the struct):

| Module | Tools | Lines | Notes |
|--------|-------|-------|-------|
| `mod.rs` | Trait, registry, shared helpers | ~300 | Core interface |
| `sandbox.rs` | `sandbox.exec` | ~1,615 | Largest single tool; heavy sandbox plumbing |
| `user_interaction.rs` | 2 user tools | ~1,303 | Largest multi-tool module; approval plumbing |
| `agent_revision.rs` | 6 revision tools + helpers | ~906 | Revision materialization, diff logic |
| `workflow.rs` | 4 workflow tools | ~916 | Task polling, approval status |
| `eval.rs` | 4 eval tools + helpers | ~754 | Suite validation, assertion engine |
| `knowledge.rs` | 6 knowledge/digest tools | ~685 | Tier-2 memory access |
| `agent.rs` | 3 agent tools | ~857 | Agent lifecycle; no further split needed |
| `artifact.rs` | 3 artifact tools | ~455 | Build, inspect, resolve |
| `web.rs` | 2 web tools + HTTP helpers | ~344 | Provider fallback, caching |
| `content.rs` | 2 content tools | ~266 | Simple CRUD |
| `session.rs` | `session.escalate` | ~199 | Single tool |
| `execution.rs` | `execution.search` | ~155 | Single tool |
| `digest.rs` | `digest.annotate` | ~98 | Single tool |

## 3. Pluggable Registration

Each module exposes a `register_tools()` function:

```rust
// tools/sandbox.rs
pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(SandboxExecTool));
}

// tools/eval.rs
pub fn register_tools(registry: &mut NativeToolRegistry) {
    registry.register(Box::new(EvalSuitePublishTool));
    registry.register(Box::new(EvalRunTool));
    registry.register(Box::new(EvalCompareTool));
    registry.register(Box::new(EvalReportTool));
}
```

`mod.rs` composes them:

```rust
pub fn default_registry() -> NativeToolRegistry {
    let mut r = NativeToolRegistry::new();
    sandbox::register_tools(&mut r);
    web::register_tools(&mut r);
    content::register_tools(&mut r);
    artifact::register_tools(&mut r);
    execution::register_tools(&mut r);
    knowledge::register_tools(&mut r);
    session::register_tools(&mut r);
    agent::register_tools(&mut r);
    agent_revision::register_tools(&mut r);
    eval::register_tools(&mut r);
    workflow::register_tools(&mut r);
    user_interaction::register_tools(&mut r);
    digest::register_tools(&mut r);
    crate::runtime::tools_promotion::register_tools(&mut r);
    r
}
```

Adding or removing a tool = edit one module's `register_tools()` — no touch to other modules.

## 4. Out of Scope for This Plan (Deferred)

The following are explicitly deferred to a separate follow-on plan because they change behavior/policy and are not part of pure modularization:

- introducing tool privilege classes (for example `AdminOnly` vs `Standard`)
- splitting runtime exposure into multiple registries with different tool visibility
- changing policy-engine semantics beyond existing capability + approval-queue checks

This modularization plan keeps a single functional registry behavior equivalent to today's runtime.

## 5. Shared Dependencies

Helpers that move to `tools/mod.rs` (used across multiple modules):

| Helper | Used by |
|--------|---------|
| `validate_agent_id()` | agent, agent_revision, eval |
| `validate_relative_agent_path()` | agent_revision |
| `tier2_memory_for_native_tool()` | knowledge |
| `capability_type_name()` | agent |
| `resolve_target_to_agent_ref()` | agent_revision |
| `extract_host()` | sandbox, web |
| `default_true()` | user_interaction |

Everything else is module-local — each tool's `*Args` structs and private helpers stay with their tool.

## 6. Execution Order

The registry types must be extracted first so that every subsequent module move can import them from `tools/mod.rs` immediately, instead of importing from the old monolith and then switching later.

### Phase 1: Extract registry and trait (medium risk)

1. Create `tools/mod.rs` with `NativeTool` trait, `NativeToolRegistry` struct, and shared helpers
2. Re-export from `tools.rs` so all existing call sites compile unchanged
3. Verify `cargo test -p autonoetic-gateway` passes

### Phase 2: Independent small modules (very low risk)

4. `digest.rs` — 98 lines
5. `execution.rs` — 155 lines
6. `session.rs` — 199 lines
7. `content.rs` — 266 lines

Self-contained, no shared helpers beyond the trait.

### Phase 3: Multi-tool modules with local helpers (low risk)

8. `web.rs` — 344 lines
9. `artifact.rs` — 455 lines
10. `knowledge.rs` — 685 lines
11. `eval.rs` — 754 lines

Module-local helpers, no cross-module dependencies.

### Phase 4: Large modules with shared helpers (medium risk)

12. `agent.rs` — 857 lines
13. `agent_revision.rs` — 906 lines
14. `workflow.rs` — 916 lines
15. `user_interaction.rs` — 1,303 lines
16. `sandbox.rs` — 1,615 lines

Most internal complexity. Shared helpers need careful extraction into `mod.rs`.

### Phase 5: Registry handoff and cleanup (low risk)

17. Move `default_registry()` into `tools/mod.rs`, delegating to per-module `register_tools()`
18. Delete old `tools.rs` monolith
19. Merge or relocate `tools_promotion.rs`
20. Update `runtime/mod.rs` declarations
21. `cargo test -p autonoetic-gateway`

## 7. Testing Strategy

Tests reference tools by name string through the registry, so they should work unchanged as long as the registry returns the same tools. After each phase:

```bash
cargo test -p autonoetic-gateway
```

Key test files:
- `tests/execution_search_integration.rs`
- `tests/tier2_memory_integration.rs`
- `tests/post_session_digest_integration.rs`
- `tests/agent_revision_*.rs` (multiple)
- `tests/eval_*.rs` (multiple)
- `tests/turn_continuation_approval_integration.rs`

## 8. Notes

- **No behavior changes** — purely code organization
- **`agent.install`** is removed from the active native tool registry; activation path is revision create + promote (or operator seed).
- **`tools_promotion.rs`** can merge into new `tools/promotion.rs` or stay as-is temporarily
