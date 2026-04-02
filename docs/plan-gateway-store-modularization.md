# Plan: Gateway Store Modularization

**Status:** Draft

**Goal:** Split `autonoetic-gateway/src/scheduler/gateway_store.rs` into smaller, topic-focused modules without changing behavior or schema semantics.

**Problem:** `gateway_store.rs` has grown into a multi-thousand-line monolith that mixes database bootstrap, schema migration, row decoding, retention logic, and many unrelated repository domains in one file. That makes review slower, increases merge conflicts, and raises the risk of accidental regressions when changing one area.

**Current file:** `autonoetic-gateway/src/scheduler/gateway_store.rs`

---

## 1. Target Structure

```text
autonoetic-gateway/src/scheduler/
  gateway_store/
    mod.rs
    db.rs
    migrate.rs
    row_decode.rs
    util.rs
    credentials.rs
    memory.rs
    runtime_control.rs
    approvals.rs
    notifications.rs
    user_interactions.rs
    artifacts.rs
    workflow.rs
    observability.rs
    agent_registry.rs
    evaluations.rs
```

### Purpose

This structure keeps `GatewayStore` as the single public facade while moving implementation details into smaller files grouped by domain. The first pass should optimize for readability and low-risk extraction, not for creating a complicated repository abstraction.

---

## 2. Domain Boundaries

### Core files

| File | Purpose |
|------|---------|
| `mod.rs` | Public module entrypoint, exports, and `GatewayStore` facade |
| `db.rs` | `GatewayStore` definition, `open()`, connection helpers, shared setup |
| `migrate.rs` | Schema creation, migration orchestration, schema versioning, backfills |
| `row_decode.rs` | Row-to-record decoding helpers shared across domains |
| `util.rs` | Small SQL and string helpers used by multiple modules |

### Domain files

| File | Purpose |
|------|---------|
| `credentials.rs` | Credential metadata CRUD |
| `memory.rs` | Tier-2 memory persistence and lookup |
| `runtime_control.rs` | Emergency stop records, active executions, stale reconciliation |
| `approvals.rs` | Approval request lifecycle |
| `notifications.rs` | Notification queue and delivery state |
| `user_interactions.rs` | User prompt/answer/cancel/expire flows |
| `artifacts.rs` | Artifact ref creation, resolution, revocation, listing |
| `workflow.rs` | Workflow events, workflow index, root-session lookup |
| `observability.rs` | Causal events, execution traces, live digest events, session transcripts, retention |
| `agent_registry.rs` | Agent revisions, aliases, session-agent bindings, promotions, short IDs |
| `evaluations.rs` | Eval suites, eval runs, eval case results |

### Purpose

The main purpose of the split is to align file boundaries with ownership boundaries. Someone working on approvals or evals should not need to mentally page through memory persistence, workflow indexing, and transcript search code in the same file.

---

## 3. Refactor Constraints

- [ ] Preserve the current public `GatewayStore` API during the first refactor pass.
- [ ] Avoid schema changes unless they are already required by unrelated functional work.
- [ ] Avoid mixing behavioral fixes with file movement.
- [ ] Keep `GatewayStore` as the only externally-used type for now.
- [ ] Keep tests green after each extraction batch.

### Purpose

These constraints keep the refactor reviewable. The first modularization pass should be organizational, not architectural. If method names, semantics, and schema all change at once, review becomes much harder and bugs become easier to miss.

---

## 4. Checklist

### Phase 1: Create the module scaffold

- [ ] Create `autonoetic-gateway/src/scheduler/gateway_store/`
- [ ] Move the existing file to `gateway_store/mod.rs`
- [ ] Update `scheduler/mod.rs` or other module declarations to use the directory module
- [ ] Verify the crate still compiles before any functional extraction

### Purpose

This phase creates the new home for the code with the least possible behavior risk. It turns the monolith into a module namespace first, so later extractions become file moves instead of invasive rewrites.

### Phase 2: Extract shared helpers

- [ ] Move `escape_sqlite_like_fragment()` into `gateway_store/util.rs`
- [ ] Move `memory_object_from_row()` and similar row-decoding helpers into `gateway_store/row_decode.rs`
- [ ] Move small record structs that are only used by one domain into their target modules
- [ ] Keep shared helpers `pub(crate)` only when needed

### Purpose

This phase reduces top-of-file noise and makes domain code easier to read. Helper extraction is low risk because it usually does not change behavior or call patterns.

### Phase 3: Extract DB bootstrap and migration logic

- [ ] Move `SCHEMA_VERSION_LATEST` to `gateway_store/migrate.rs`
- [ ] Move `migrate()` to `gateway_store/migrate.rs`
- [ ] Move schema creation SQL into domain-oriented helper functions inside `migrate.rs`
- [ ] Move `backfill_workflow_index()` into `gateway_store/workflow.rs` or keep it in `migrate.rs` if it is migration-only
- [ ] Keep `GatewayStore::open()` in `db.rs`, delegating to migration helpers
- [ ] Keep stale execution reconciliation delegated from `open()`

### Purpose

Migration/bootstrap code is a different concern from runtime repository logic. Separating it makes the main store implementation much smaller and makes schema review easier in future changes.

### Phase 4: Extract self-contained repository domains first

- [ ] Move credential methods into `gateway_store/credentials.rs`
- [ ] Move memory methods into `gateway_store/memory.rs`
- [ ] Move artifact-ref methods into `gateway_store/artifacts.rs`
- [ ] Ensure each new file contains only its own `impl GatewayStore` block(s)
- [ ] Keep imports local to each module instead of re-exporting everything through `mod.rs`

### Purpose

These domains are relatively independent and make good low-risk wins. Extracting them first shrinks the monolith quickly and validates the modularization pattern before touching more coupled areas.

### Phase 5: Extract human-in-the-loop domains

- [ ] Move approval methods into `gateway_store/approvals.rs`
- [ ] Move notification methods into `gateway_store/notifications.rs`
- [ ] Move user interaction methods into `gateway_store/user_interactions.rs`
- [ ] Keep approval/notification/user-interaction boundaries explicit even if they share helper patterns

### Purpose

These systems are related operationally but not identical conceptually. Grouping them as separate files keeps approval logic from collapsing into another mini-monolith.

### Phase 6: Extract workflow and runtime-control domains

- [ ] Move workflow event and workflow index methods into `gateway_store/workflow.rs`
- [ ] Move emergency-stop and active-execution methods into `gateway_store/runtime_control.rs`
- [ ] Move host/process reconciliation helpers alongside active execution code
- [ ] Re-check transaction boundaries after extraction

### Purpose

These areas are slightly more coupled because workflow and runtime state often interact. They should be extracted together only after the lower-risk modules have already proven out the structure.

### Phase 7: Extract observability and search domains

- [ ] Move causal event methods into `gateway_store/observability.rs`
- [ ] Move execution trace methods into `gateway_store/observability.rs`
- [ ] Move live digest methods into `gateway_store/observability.rs`
- [ ] Move session transcript / FTS methods into `gateway_store/observability.rs`
- [ ] Move retention helpers (`prune_execution_traces`, `prune_causal_events`, `apply_retention_policy`) into the same file

### Purpose

Observability concerns all write to operational history tables, and they are often changed together. Keeping them in one file makes search/trace/retention behavior easier to reason about without mixing them with unrelated workflow or agent revision code.

### Phase 8: Extract agent lifecycle and release-management domains

- [ ] Move agent revision methods into `gateway_store/agent_registry.rs`
- [ ] Move alias and session-agent binding methods into `gateway_store/agent_registry.rs`
- [ ] Move promotion methods into `gateway_store/agent_registry.rs`
- [ ] Move short-ID methods into `gateway_store/agent_registry.rs`
- [ ] Move eval suite/run/result methods into `gateway_store/evaluations.rs`

### Purpose

These methods are among the most transactional and structurally dense in the file. They should be moved later, once the module pattern is stable and reviewers are not also trying to validate the new layout at the same time.

### Phase 9: Split tests by domain

- [ ] Move `gateway_store.rs` tests into domain-aligned test modules
- [ ] Keep shared test fixtures in one helper module
- [ ] Add missing domain-focused unit tests where modularization reveals coverage gaps
- [ ] Ensure tests reference the same public `GatewayStore` API

### Purpose

Test modularization keeps maintenance burden from shifting from one giant source file to one giant test block. It also makes it easier to see which domains are under-tested.

### Phase 10: Cleanup and size review

- [ ] Remove unused imports and helpers left behind in `mod.rs`
- [ ] Confirm no extracted file has turned into a new monolith
- [ ] Check final module sizes and rebalance if one file remains too large
- [ ] Add a short architecture note if the final structure differs from this plan

### Purpose

A modularization is only successful if it actually improves code navigation. This phase makes sure the result is not just the same complexity spread across slightly different filenames.

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
16. split tests

### Purpose

This order starts with low-risk infrastructure and small domains, then moves toward workflow, observability, and transactional domains. That keeps the early PRs simple and reduces the chance of destabilizing the file all at once.

---

## 6. Validation Checklist

- [ ] `cargo test -p autonoetic-gateway --lib`
- [ ] `cargo test -p autonoetic-gateway`
- [ ] Spot-check schema initialization on a fresh `.gateway/gateway.db`
- [ ] Spot-check opening an existing gateway DB to ensure migrations/backfills still run
- [ ] Spot-check one method from each extracted module after its move

### Purpose

Modularization bugs often show up as missing imports, broken helper visibility, or accidental transaction changes rather than obvious compile failures. This checklist keeps validation concrete.

---

## 7. First PR Recommendation

- [ ] Create `gateway_store/`
- [ ] Move current file to `gateway_store/mod.rs`
- [ ] Extract `util.rs`
- [ ] Extract `row_decode.rs`
- [ ] Extract `migrate.rs`
- [ ] Leave all repository methods in `mod.rs` for that first PR

### Purpose

This first PR gives a meaningful structural win without forcing reviewers to validate every repository move at the same time. It creates the new module system and removes the biggest non-domain chunk, making follow-up PRs smaller and safer.

---

## 8. Notes

- The first pass should favor simple `impl GatewayStore` extraction over introducing many new repository wrapper types.
- If a later pass wants domain-specific store structs such as `ApprovalStore` or `MemoryStore`, that should be a separate design change after the file split is complete.
- Do not bundle unrelated feature work into the modularization PRs.
