# Plan: Network Approval Detection & Packager/Layer Integration Fix

**Date:** 2026-04-05
**Status:** Phase 1–3 Complete; Phase 4 Future
**Related:** `docs/remote-access-approval.md`, `docs/plan-build-layers-dependency-resolution.md`, `docs/plan-capability-driven-sandbox-isolation.md`

---

## Problem

Demo session `demo-session-1` failed because network approvals never fire and the builder/layer workflow is broken:

1. **`RemoteAccessAnalyzer` misses critical cases** — only scans command text; ignores `dependencies.packages`, network commands (`pip install`, `curl`), transitive imports, and non-entrypoint artifact files
2. **Packager agent lacks `NetworkAccess` capability** — can't install deps even when invoked
3. **Planner doesn't delegate to builder** — goes straight from coder → evaluator, skipping the builder step that the layer architecture was designed for
4. **Packager artifacts should not include `dependencies`** — when deps are in layers, the `dependencies` field must be absent so `compose_entrypoint` skips pip install
5. **`AnalysisProvider` trait unused in sandbox** — pluggable analysis exists but is wired for `agent.install` only

### Design Principle

**Gateway = dumb mechanism enforcement.** It detects capabilities, gates approvals, enforces isolation. It does NOT make intelligent decisions about specific cases. Intelligence lives in agents (SKILL.md). See `docs/gateway-architecture-principles.md`.

---

## Phase 1: Fix RemoteAccessAnalyzer — Generic Detection

**File:** `autonoetic-gateway/src/runtime/remote_access.rs`

### Task 1.1: Add `detect_network_commands()` method

- [x] New detection category `network_command` with generic patterns:
  - Package managers: `pip install`, `pip3 install`, `npm install`, `yarn install/add`, `pnpm install`, `bun install`, `go get`, `go mod download`, `cargo install`, `gem install`, `composer install/require`
  - Download tools: `curl`, `wget`
  - System pkg managers: `apt-get install/update`, `apk add`, `yum install`, `dnf install`, `pacman -S`
  - VCS network ops: `git clone`, `git fetch`, `git pull`, `git push`
- [x] Integrate into `analyze_code()` as 5th detection pass
- [x] Tests: `test_pip_install_detected`, `test_npm_install_detected`, `test_curl_detected`, `test_git_clone_detected`, `test_wget_detected`, `test_apt_get_install_detected`

### Task 1.2: Add `analyze_command_and_dependencies()` method

- [x] New public method accepting code + optional dep packages
- [x] If `dep_packages` is Some and non-empty → adds synthetic `DetectedPattern { category: "dependency_install" }`
- [x] Merges with `analyze_code()` results
- [x] Tests: `test_dependencies_imply_network`, `test_dependencies_empty_no_flag`, `test_dependencies_none_no_flag`, `test_dependencies_combine_with_code_patterns`

### Task 1.3: Add `analyze_code_with_workspace()` — transitive imports

- [x] Parse `import X` / `from X import` from primary code
- [x] Match module names against workspace filenames (`import mymod` → `mymod.py`)
- [x] Analyze each matched file, merge results (union of patterns)
- [x] Tests: `test_transitive_import_detected`, `test_transitive_no_match`, `test_transitive_empty_workspace`

---

## Phase 2: Wire Detection into sandbox.rs — Mechanism Only

**File:** `autonoetic-gateway/src/runtime/tools/sandbox.rs`

### Task 2.1: Dependency-aware analysis

- [x] Replace all `RemoteAccessAnalyzer::analyze_code()` calls with `analyze_command_and_dependencies()`
- [x] Extract `dep_packages` from `args.dependencies` before the move into `dependency_plan_from_args_or_lock`
- [x] Pass through to all analysis call sites (5 total)

### Task 2.2: Full artifact file analysis

- [x] Change artifact remote access check from iterating `bundle.entrypoints` to ALL `bundle.files`
- [x] Each file prefixed with `# --- {filename} ---` header for traceability

### Task 2.3: Cache fingerprints guard — `has_concrete_targets`

- [x] Added `has_concrete_targets()` guard on cache lookup
- [x] Import-only patterns (no URLs/IPs) always require re-approval — cache bypassed
- [x] Prevents false cache hits from opaque patterns that can resolve to different targets

### Task 2.4: Cache fingerprints include dependencies

**File:** `autonoetic-gateway/src/runtime/approved_exec_cache.rs`

- [ ] Extend `compute_fingerprint()` with `dep_packages: Option<&[String]>` parameter
- [ ] Include dep_packages in SHA256 hash input
- [ ] Update all callers in sandbox.rs

*(Deferred — current fingerprint is sufficient since normalized_targets only use URL/IP. Dep packages are already represented via the `dependency_install` detected pattern.)*

---

## Phase 3: Fix Packager Agent & Layer Workflow — Agent Intelligence

**No gateway code changes.** The gateway already mounts layers and respects absent `dependencies`. The fix is in agent SKILL.md files.

### Task 3.1: Add `NetworkAccess` to packager capabilities

**File:** `agents/specialists/packager.default/SKILL.md`

- [x] Add `NetworkAccess` capability to YAML frontmatter:
  ```yaml
  - type: "NetworkAccess"
    hosts: ["*"]
  ```
- [x] Removed stale `sandbox.conf share_net = true` references
- [x] This makes `BwrapIsolationOverrides::from_capabilities()` return `share_net: true` automatically

### Task 3.2: Packager workflow: layered artifacts WITHOUT `dependencies`

**File:** `agents/specialists/packager.default/SKILL.md`

- [x] Added CRITICAL note: packager must NOT include `dependencies` field in layered artifacts
- [x] Documented that `mount_path` should match the `--target` path used in sandbox.exec
- [x] Added entrypoint setup guidance (PYTHONPATH for Python, NODE_PATH for Node.js)
- [x] Gateway already handles this correctly — no code changes needed

### Task 3.3: Strengthen planner's packager delegation

**File:** `agents/lead/planner.default/SKILL.md`

- [x] Replaced vague decision flow rule with HARD delegation rules:
  - Any artifact with `requirements.txt`, `package.json`, `pyproject.toml`, `go.mod`, `Cargo.toml`
  - Code using network libraries (`import requests`, `import httpx`, etc.)
  - `sandbox.exec` including `dependencies: {packages: [...]}`
- [x] Packager MUST happen between coder and evaluator — **NEVER skip when deps exist**
- [x] Updated dependencies section with explanation of why packager is mandatory

---

## Phase 4: Bridge to Pluggable AnalysisProvider (Future)

Architectural cleanup — makes `code_analysis` config affect `sandbox.exec`, not just `agent.install`.

### Task 4.1: RemoteAccessAnalysisProvider adapter

**New file:** `autonoetic-gateway/src/runtime/analysis/remote_access_adapter.rs`

- [ ] Implement `AnalysisProvider` trait, delegating to `RemoteAccessAnalyzer`
- [ ] Map `DetectedPattern` → `CapabilityEvidence`, categories → capability types

### Task 4.2: Sandbox factory method

**File:** `autonoetic-gateway/src/runtime/analysis/mod.rs`

- [ ] `create_for_sandbox(provider_type)` factory method
- [ ] Wraps configured provider with `RemoteAccessAnalysisProvider`

### Task 4.3: Unify in sandbox.rs

- [ ] New `analyze_sandbox_exec()` function using the provider
- [ ] Replace all `RemoteAccessAnalyzer` call sites
- [ ] Falls back to direct `RemoteAccessAnalyzer` if no config

---

## Verification

- [x] `cargo test -p autonoetic-gateway` — all 418+ tests pass, 0 failures
- [x] New unit tests for all new detection methods pass (22 total in remote_access)
- [ ] Replay weather agent demo → full pipeline completes:
  - Coder produces files with `requirements.txt`
  - Planner delegates to packager (not evaluator)
  - Packager installs deps (network works), captures layers
  - Packager builds artifact WITHOUT `dependencies` field
  - Evaluator runs offline — layers provide deps
  - No 50-turn loops
- [ ] Non-layered path: `dependencies: {packages: ["requests"]}` → approval triggered → approved → `share_net: true` → success
- [ ] Update `docs/remote-access-approval.md` with new detection categories
