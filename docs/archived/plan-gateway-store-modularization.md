# Plan: Gateway Store Modularization

**Status:** Complete

**Goal:** Split `autonoetic-gateway/src/scheduler/gateway_store.rs` into smaller, topic-focused modules without changing behavior or schema semantics.

**Problem:** `gateway_store.rs` has grown into a multi-thousand-line monolith that mixes database bootstrap, schema migration, row decoding, retention logic, and many unrelated repository domains in one file. That makes review slower, increases merge conflicts, and raises the risk of accidental regressions when changing one area.

**Result:** The monolith has been split into 15 topic-focused modules. The `mod.rs` facade is ~550 lines. All 401 tests pass.

---

## 1. Target Structure

```text
autonoetic-gateway/src/scheduler/
  gateway_store/
    mod.rs               # GatewayStore struct, open(), tests (~550 lines)
    agent_registry.rs    # Agent revisions, aliases, bindings, promotions, short IDs (~665 lines)
    approvals.rs         # Approval request lifecycle (~210 lines)
    artifacts.rs         # Artifact ref creation, resolution, revocation, listing (~205 lines)
    credentials.rs       # Credential metadata CRUD (~125 lines)
    evaluations.rs       # Eval suites, eval runs, eval case results (~295 lines)
    memory.rs            # Tier-2 memory persistence and lookup (~225 lines)
    migrate.rs           # Schema creation, migration orchestration, backfills (~560 lines)
    notifications.rs     # Notification queue and delivery state (~200 lines)
    observability.rs     # Causal events, execution traces, live digest, session transcripts, retention (~500 lines)
    row_decode.rs        # Row-to-record decoding helpers (~60 lines)
    runtime_control.rs   # Emergency stop records, active executions (~190 lines)
    user_interactions.rs # User prompt/answer/cancel/expire flows (~345 lines)
    util.rs              # Small SQL and string helpers (~15 lines)
    workflow.rs          # Workflow events, workflow index (~145 lines)
```

### Purpose

This structure keeps `GatewayStore` as the single public facade while moving implementation details into smaller files grouped by domain. The first pass optimizes for readability and low-risk extraction, not for creating a complicated repository abstraction.

---

## 2. Domain Boundaries

### Core files

| File | Purpose | Status |
|------|---------|--------|
| `mod.rs` | Public module entrypoint, exports, and `GatewayStore` facade | Done |
| `migrate.rs` | Schema creation, migration orchestration, schema versioning, backfills | Done |
| `row_decode.rs` | Row-to-record decoding helpers shared across domains | Done |
| `util.rs` | Small SQL and string helpers used by multiple modules | Done |

### Domain files

| File | Purpose | Status |
|------|---------|--------|
| `credentials.rs` | Credential metadata CRUD | Done |
| `memory.rs` | Tier-2 memory persistence and lookup | Done |
| `runtime_control.rs` | Emergency stop records, active executions, stale reconciliation | Done |
| `approvals.rs` | Approval request lifecycle | Done |
| `notifications.rs` | Notification queue and delivery state | Done |
| `user_interactions.rs` | User prompt/answer/cancel/expire flows | Done |
| `artifacts.rs` | Artifact ref creation, resolution, revocation, listing | Done |
| `workflow.rs` | Workflow events, workflow index, root-session lookup | Done |
| `observability.rs` | Causal events, execution traces, live digest events, session transcripts, retention | Done |
| `agent_registry.rs` | Agent revisions, aliases, session-agent bindings, promotions, short IDs | Done |
| `evaluations.rs` | Eval suites, eval runs, eval case results | Done |

### Purpose

The main purpose of the split is to align file boundaries with ownership boundaries. Someone working on approvals or evals should not need to mentally page through memory persistence, workflow indexing, and transcript search code in the same file.

---

## 3. Refactor Constraints

- [x] Preserve the current public `GatewayStore` API during the first refactor pass.
- [x] Avoid schema changes unless they are already required by unrelated functional work.
- [x] Avoid mixing behavioral fixes with file movement.
- [x] Keep `GatewayStore` as the only externally-used type for now.
- [x] Keep tests green after each extraction batch.

### Purpose

These constraints keep the refactor reviewable. The first modularization pass should be organizational, not architectural. If method names, semantics, and schema all change at once, review becomes much harder and bugs become easier to miss.

---

## 4. Checklist

### Phase 1: Create the module scaffold

- [x] Create `autonoetic-gateway/src/scheduler/gateway_store/`
- [x] Move the existing file to `gateway_store/mod.rs`
- [x] Update `scheduler/mod.rs` or other module declarations to use the directory module
- [x] Verify the crate still compiles before any functional extraction

### Phase 2: Extract shared helpers

- [x] Move `escape_sqlite_like_fragment()` into `gateway_store/util.rs`
- [x] Move `memory_object_from_row()` and similar row-decoding helpers into `gateway_store/row_decode.rs`
- [x] Move small record structs that are only used by one domain into their target modules
- [x] Keep shared helpers `pub(crate)` only when needed

### Phase 3: Extract DB bootstrap and migration logic

- [x] Move `SCHEMA_VERSION_LATEST` to `gateway_store/migrate.rs`
- [x] Move `migrate()` to `gateway_store/migrate.rs`
- [x] Move schema creation SQL into domain-oriented helper functions inside `migrate.rs`
- [x] Move `backfill_workflow_index()` into `gateway_store/migrate.rs`
- [x] Keep `GatewayStore::open()` in `mod.rs`, delegating to migration helpers
- [x] Keep stale execution reconciliation delegated from `open()`

### Phase 4: Extract self-contained repository domains first

- [x] Move credential methods into `gateway_store/credentials.rs`
- [x] Move memory methods into `gateway_store/memory.rs`
- [x] Move artifact-ref methods into `gateway_store/artifacts.rs`
- [x] Ensure each new file contains only its own `impl GatewayStore` block(s)
- [x] Keep imports local to each module instead of re-exporting everything through `mod.rs`

### Phase 5: Extract human-in-the-loop domains

- [x] Move approval methods into `gateway_store/approvals.rs`
- [x] Move notification methods into `gateway_store/notifications.rs`
- [x] Move user interaction methods into `gateway_store/user_interactions.rs`
- [x] Keep approval/notification/user-interaction boundaries explicit even if they share helper patterns

### Phase 6: Extract workflow and runtime-control domains

- [x] Move workflow event and workflow index methods into `gateway_store/workflow.rs`
- [x] Move emergency-stop and active-execution methods into `gateway_store/runtime_control.rs`
- [x] Move host/process reconciliation helpers alongside active execution code
- [x] Re-check transaction boundaries after extraction

### Phase 7: Extract observability and search domains

- [x] Move causal event methods into `gateway_store/observability.rs`
- [x] Move execution trace methods into `gateway_store/observability.rs`
- [x] Move live digest methods into `gateway_store/observability.rs`
- [x] Move session transcript / FTS methods into `gateway_store/observability.rs`
- [x] Move retention helpers (`prune_execution_traces`, `prune_causal_events`, `apply_retention_policy`) into the same file

### Phase 8: Extract agent lifecycle and release-management domains

- [x] Move agent revision methods into `gateway_store/agent_registry.rs`
- [x] Move alias and session-agent binding methods into `gateway_store/agent_registry.rs`
- [x] Move promotion methods into `gateway_store/agent_registry.rs`
- [x] Move short-ID methods into `gateway_store/agent_registry.rs`
- [x] Move eval suite/run/result methods into `gateway_store/evaluations.rs`

### Phase 9: Split tests by domain

- [ ] Move `gateway_store.rs` tests into domain-aligned test modules
- [ ] Keep shared test fixtures in one helper module
- [ ] Add missing domain-focused unit tests where modularization reveals coverage gaps
- [ ] Ensure tests reference the same public `GatewayStore` API

### Phase 10: Cleanup and size review

- [x] Remove unused imports and helpers left behind in `mod.rs`
- [x] Confirm no extracted file has turned into a new monolith
- [x] Check final module sizes and rebalance if one file remains too large
- [x] Add a short architecture note if the final structure differs from this plan

---

## 5. Recommended Extraction Order

1. Module scaffold (`mod.rs`)
2. `util.rs`
3. `row_decode.rs`
4. `migrate.rs`
5. `credentials.rs`
6. `memory.rs`
7. `artifacts.rs`
8. `approvals.rs`
9. `notifications.rs`
10. `user_interactions.rs`
11. `workflow.rs`
12. `runtime_control.rs`
13. `observability.rs`
14. `agent_registry.rs`
15. `evaluations.rs`
16. split tests (deferred)

### Purpose

This order starts with low-risk infrastructure and small domains, then moves toward workflow, observability, and transactional domains. That keeps the early PRs simple and reduces the chance of destabilizing the file all at once.

---

## 6. Validation Checklist

- [x] `cargo test -p autonoetic-gateway --lib` (401 tests pass)
- [x] `cargo test -p autonoetic-gateway` (all tests pass)
- [x] Spot-check schema initialization on a fresh `runtime/gateway.db`
- [x] Spot-check opening an existing gateway DB to ensure migrations/backfills still run
- [x] Spot-check one method from each extracted module after its move

### Purpose

Modularization bugs often show up as missing imports, broken helper visibility, or accidental transaction changes rather than obvious compile failures. This checklist keeps validation concrete.

---

## 7. First PR Recommendation

- [x] Create `gateway_store/`
- [x] Move current file to `gateway_store/mod.rs`
- [x] Extract `util.rs`
- [x] Extract `row_decode.rs`
- [x] Extract `migrate.rs`
- [x] Extract all domain modules

### Purpose

The full modularization was completed in one pass, with compilation verified after each extraction batch.

---

## 8. Notes

- The first pass favors simple `impl GatewayStore` extraction over introducing many new repository wrapper types.
- If a later pass wants domain-specific store structs such as `ApprovalStore` or `MemoryStore`, that should be a separate design change after the file split is complete.
- Do not bundle unrelated feature work into the modularization PRs.
- Phase 9 (split tests by domain) is deferred — tests remain in `mod.rs` for now.
