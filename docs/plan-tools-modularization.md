# Plan: Tools Modularization

**Status:** ✅ **COMPLETE** — Archived 2026-04-01

**Triggered by:** [plan-agent-revision-evaluation-federation-mvp.md](plan-agent-revision-evaluation-federation-mvp.md) — "Keep gateway tooling modularization as a post-MVP maintenance task. Split `autonoetic-gateway/src/runtime/tools.rs` into smaller topic-focused modules once the MVP tool surface stabilizes."

**Before:** `autonoetic-gateway/src/runtime/tools.rs` — 8,863 lines, 36 tools + helpers + registry in one file.

**After:** 14 topic-focused modules + `default_registry()` in `tools/mod.rs`. Zero monolith files remain. Commits: `19191ff` (initial extraction), `e806994` (final cleanup).

---

## 1. Final Structure

```
autonoetic-gateway/src/runtime/
  tools/
    mod.rs                 # NativeTool trait, NativeToolRegistry, default_registry(), shared helpers, InstallAgentFile (~420 lines)
    sandbox.rs             # sandbox.exec (~1,050 lines)
    web.rs                 # web.search, web.fetch (~340 lines)
    content.rs             # content.write, content.read (~260 lines)
    artifact.rs            # artifact.build, artifact.inspect, artifact.resolve_ref (~450 lines)
    execution.rs           # execution.search (~147 lines)
    knowledge.rs           # knowledge.store/.recall/.search/.search_by_tags/.share, digest.query (~680 lines)
    session.rs             # session.escalate (~189 lines)
    agent.rs               # agent.spawn, agent.exists, agent.discover (~450 lines)
    agent_revision.rs      # agent.revision.{create,list,inspect,promote,rollback,diff} (~1,030 lines)
    evaluation.rs          # eval.{suite.publish,run,compare,report} (~704 lines)
    workflow.rs            # approval.status, workflow.{wait,state,cancel_task} (~900 lines)
    user_interaction.rs    # user.ask, user.interaction.status (~350 lines)
    digest.rs              # digest.annotate (~107 lines)
    promotion.rs           # promotion.record, promotion.query (~250 lines)
  tests/
    native_tool_registry_tests.rs  # Registry availability, web.fetch/search, agent.spawn tests (~880 lines)
```

## 2. Module Sizes (Actual)

| Module | Tools | Lines | Notes |
|--------|-------|-------|-------|
| `mod.rs` | Trait, registry, default_registry(), shared helpers | ~420 | Core interface + sandbox helper types |
| `sandbox.rs` | `sandbox.exec` | ~1,050 | Heavy sandbox plumbing |
| `agent_revision.rs` | 6 revision tools + helpers | ~1,030 | Revision materialization, diff logic |
| `workflow.rs` | 4 workflow tools | ~900 | Task polling, approval status |
| `evaluation.rs` | 4 eval tools + helpers | ~704 | Suite validation, assertion engine |
| `knowledge.rs` | 6 knowledge/digest tools | ~680 | Tier-2 memory access |
| `artifact.rs` | 3 artifact tools | ~450 | Build, inspect, resolve |
| `agent.rs` | 3 agent tools | ~450 | Agent lifecycle |
| `user_interaction.rs` | 2 user tools | ~350 | Approval plumbing |
| `web.rs` | 2 web tools + HTTP helpers | ~340 | Provider fallback, caching |
| `promotion.rs` | 2 promotion tools | ~250 | Artifact promotion gating |
| `content.rs` | 2 content tools | ~260 | Simple CRUD |
| `session.rs` | `session.escalate` | ~189 | Single tool |
| `execution.rs` | `execution.search` | ~147 | Single tool |
| `digest.rs` | `digest.annotate` | ~107 | Single tool |

**Total reduction:** 8,863 → 0 lines in monolith (100% elimination).

## 3. Pluggable Registration

Each module exposes a `register_tools()` function. `default_registry()` in `tools/mod.rs` composes them all:

```rust
pub fn default_registry() -> NativeToolRegistry {
    let mut registry = NativeToolRegistry::new();
    crate::runtime::tools::execution::register_tools(&mut registry);
    crate::runtime::tools::digest::register_tools(&mut registry);
    crate::runtime::tools::session::register_tools(&mut registry);
    crate::runtime::tools::content::register_tools(&mut registry);
    crate::runtime::tools::agent_revision::register_tools(&mut registry);
    crate::runtime::tools::evaluation::register_tools(&mut registry);
    crate::runtime::tools::web::register_tools(&mut registry);
    crate::runtime::tools::artifact::register_tools(&mut registry);
    crate::runtime::tools::knowledge::register_tools(&mut registry);
    crate::runtime::tools::agent::register_tools(&mut registry);
    crate::runtime::tools::sandbox::register_tools(&mut registry);
    crate::runtime::tools::workflow::register_tools(&mut registry);
    crate::runtime::tools::user_interaction::register_tools(&mut registry);
    crate::runtime::tools::promotion::register_tools(&mut registry);
    registry
}
```

Adding or removing a tool = edit one module's `register_tools()` — no touch to other modules.

## 4. Out of Scope (Deferred)

All original scope items are now complete. No remaining deferred items from this plan.

## 5. Shared Dependencies

Helpers extracted to `tools/mod.rs` (used across multiple modules):

| Helper | Used by |
|--------|---------|
| `validate_agent_id()` | agent, agent_revision, evaluation |
| `validate_relative_agent_path()` | agent_revision |
| `tier2_memory_for_native_tool()` | knowledge |
| `capability_type_name()` | agent |
| `resolve_target_to_agent_ref()` | agent_revision |
| `extract_host()` | sandbox, web |
| `default_true()` | user_interaction |
| `build_approval_details()` | sandbox, workflow |
| `load_session_content_mounts()` | sandbox |
| `dependency_plan_from_args_or_lock()` | sandbox |
| `block_on_http()` | web |

Sandbox helper types (`SandboxExecArgs`, `SandboxExecDependencies`, `CapturePath`) also live in `mod.rs` for cross-module access.

## 6. Execution Summary

### Phase 1: Extract registry and trait — ✅ DONE
- Created `tools/mod.rs` with `NativeTool` trait, `NativeToolRegistry`, shared helpers

### Phase 2: Independent small modules — ✅ DONE
- `digest.rs`, `execution.rs`, `session.rs`, `content.rs`

### Phase 3: Multi-tool modules with local helpers — ✅ DONE
- `web.rs`, `artifact.rs`, `knowledge.rs`, `evaluation.rs`

### Phase 4: Large modules with shared helpers — ✅ DONE
- `agent.rs`, `agent_revision.rs`, `workflow.rs`, `user_interaction.rs`, `sandbox.rs`

### Phase 5: Registry handoff and cleanup — ✅ DONE
- `default_registry()` moved to `tools/mod.rs`
- `tools_promotion.rs` merged into `tools/promotion.rs`
- `InstallAgentFile` moved to `tools/mod.rs`
- Tests extracted to `tests/native_tool_registry_tests.rs`
- `tools_impl.rs` deleted entirely
- `runtime/mod.rs` declarations cleaned up
- All 363 tests pass

## 7. Testing Strategy

Tests reference tools by name string through the registry, so they work unchanged as long as the registry returns the same tools.

```bash
cargo test -p autonoetic-gateway  # 363 tests, all passing
```

Key test files:
- `tests/native_tool_registry_tests.rs` (registry availability, web tools, agent.spawn)
- `tests/execution_search_integration.rs`
- `tests/tier2_memory_integration.rs`
- `tests/post_session_digest_integration.rs`
- `tests/agent_revision_*.rs` (multiple)
- `tests/eval_*.rs` (multiple)
- `tests/turn_continuation_approval_integration.rs`

## 8. Notes

- **No behavior changes** — purely code organization
- **`agent.install`** is removed from the active native tool registry; activation path is revision create + promote (or operator seed)
- Commits: `19191ff` (extract tools into modules), `e806994` (delete monolith, merge promotion, move tests)
