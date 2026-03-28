# Plan: Capability-Driven Sandbox Isolation

**Spec:** `docs/spec-capability-driven-sandbox-isolation.md`
**Depends on:** Build Layers (complete)

---

## Phase 1: Per-Execution Sandbox Overrides

### Task 1.1: `BwrapIsolationOverrides` type

**File:** `autonoetic-gateway/src/sandbox.rs`

- [x] Add `BwrapIsolationOverrides { share_net: bool }` (default: `share_net: false`)
- [x] Add `isolation_overrides_from_capabilities(caps: &[Capability]) -> BwrapIsolationOverrides` — returns `share_net: true` if any `Capability::NetworkAccess { .. }` present
- [x] Change `append_bwrap_isolation_flags(argv: &mut Vec<String>)` → `append_bwrap_isolation_flags(argv: &mut Vec<String>, overrides: Option<&BwrapIsolationOverrides>)`
- [x] When `overrides.share_net == true`, append `--share-net` after `--unshare-all`
- [x] When `overrides` is `None`, use global `SANDBOX_CONFIG` as before (backward compat)

### Task 1.2: Thread overrides through sandbox spawn

**File:** `autonoetic-gateway/src/sandbox.rs`

- [x] Add `overrides: Option<&BwrapIsolationOverrides>` param to `spawn_with_driver_and_dependencies`
- [x] Add `overrides: Option<&BwrapIsolationOverrides>` param to `spawn_with_session_content`
- [x] Thread overrides through to `bubblewrap_command` and `bubblewrap_shell_command`
- [x] All existing callers pass `None` (no behavior change)

### Task 1.3: `sandbox.exec` passes capability-derived overrides

**File:** `autonoetic-gateway/src/runtime/tools.rs`

- [x] In `SandboxExecTool::execute()`, compute overrides from `manifest.capabilities`
- [x] Pass overrides to sandbox spawn

### Task 1.4: Script agent execution passes overrides

**File:** `autonoetic-gateway/src/execution.rs`

- [x] **Root cause**: The `execute_script_in_sandbox` function (line 2709) used local `bubblewrap_command`/`docker_command`/`microvm_command` functions that were completely separate from `sandbox.rs::SandboxRunner`. The local `bubblewrap_command` didn't invoke bwrap — it ran scripts directly with python3/node/ruby.
- [x] **Fix**: Refactored `execute_script_in_sandbox` to use `SandboxRunner::spawn_with_driver_and_dependencies` from `sandbox.rs`, deriving `BwrapIsolationOverrides` from the agent's capabilities. Removed the dead local sandbox command functions.
- [x] Script agents spawned via `agent.spawn` now get proper bwrap isolation with capability-driven network access.

---

## Phase 2: Fix Planner Builder Invocation

### Task 2.1: Add mandatory Step 2a to planner

**File:** `agents/lead/planner.default/SKILL.md`

- [x] Add explicit **Step 2a** between Step 2 (coder) and Step 2b (artifact fallback):
  ```
  Step 2a: If the artifact contains dependency files (requirements.txt, package.json, etc.),
           delegate to builder.default to install deps and create a layered artifact.
           Use the new layered artifact_id for all subsequent steps.
  ```

---

## Phase 3: Cleanup

### Task 3.1: Remove dead sandbox.conf

- [x] Delete `agents/specialists/builder.default/sandbox.conf` (not read by any code)

---

## Phase 4: Integration Tests

### Task 4.1: Capability-driven isolation test

**File:** `autonoetic-gateway/tests/capability_isolation_integration.rs` (new)

- [ ] Agent with `NetworkAccess` → `append_bwrap_isolation_flags` includes `--share-net`
- [ ] Agent without `NetworkAccess` → no `--share-net`
- [ ] Overrides `None` → falls back to global config
- [ ] `isolation_overrides_from_capabilities` unit tests

### Task 4.2: Script agent network test

**File:** `autonoetic-gateway/tests/capability_isolation_integration.rs`

- [ ] Script agent with `NetworkAccess` capability can reach network in sandbox
- [ ] Script agent without `NetworkAccess` cannot reach network

---

## Execution Order

```
Phase 1 (sandbox overrides)    ← core mechanism, no agent changes
  ├── 1.1 BwrapIsolationOverrides type          ✅
  ├── 1.2 Thread through spawn                  ✅
  ├── 1.3 sandbox.exec integration              ✅
  └── 1.4 Script agent integration              ✅

Phase 2 (planner fix)          ← Phase 1 not required, can parallel
  └── 2.1 Add Step 2a                           ✅

Phase 3 (cleanup)              ← after Phase 1
  └── 3.1 Remove dead sandbox.conf              ✅

Phase 4 (tests)                ← after Phase 1
  ├── 4.1 Capability isolation                  ⏳
  └── 4.2 Script agent network                  ⏳
```

**Estimated tasks:** 12
**Completed:** 10 (Phases 1-3 + 1.4 refactor)
**Remaining:** 2 (Phase 4 integration tests)
**Critical path:** ✅ complete — Phases 1-3 done, 1.4 done, Phase 4 optional
