# Plan: Artifact Execution, Approval Reuse, and Script-Agent Promotion

**Date:** 2026-04-17
**Status:** Complete (Phases 1–5)
**Related:** `docs/approval-system.md`, `docs/remote-access-approval.md`, `docs/AGENTS.md`, `docs/agent_routing_and_roles.md`, `docs/plan-capability-driven-sandbox-isolation.md`, `docs/plan-network-approval-builder-layers-fix.md`

---

## Problem

The current runtime mixes two different execution models:

1. **Transient script execution** via `sandbox.exec` / `executor.default`
2. **Durable reusable execution** via promoted agent revisions

This causes awkward approval behavior for artifacts that are already structured like reusable tools:

- approval reuse is keyed too heavily on command text instead of the executed artifact
- runtime remote-access analysis treats some weak signals as approval-worthy even when no host can be derived
- subsequent executions in fresh sessions can fail because approval reuse, target extraction, and sandbox network enablement are not driven by the same stable identity
- planners can keep delegating execution to `executor.default` even after an artifact has crossed the line from temporary output to reusable capability

### Concrete Failure Mode

In demo session `demo-session-1`, a built weather artifact succeeded once and then failed on a later run because:

1. the first successful run went through approval on a command shape that exposed a concrete host (`wttr.in`)
2. a later run used a different command shape that did not expose the same concrete host at the approval boundary
3. approval reuse and sandbox network configuration were therefore not applied from a stable artifact-bound identity

The result is a system that is technically safe but not idiomatic: reusable code is still treated like an opaque shell command.

---

## Design Decision

The runtime should distinguish two first-class paths:

1. **Transient artifact execution**
   For one-off validation, smoke tests, and ad hoc runs. Approval reuse should bind to the artifact + entrypoint + concrete targets, not to a raw shell string.

2. **Durable script-agent execution**
   For reusable capabilities. Once an artifact has a stable entrypoint, purpose, and external capability profile, it should be converted into a script agent revision and run via `execution_mode: script`.

### Non-Goal

Do **not** broadly relax generic `sandbox.exec` policy so the executor becomes the durable trust anchor for reusable code. That would work against the separation-of-powers model and keep policy tied to shell-shape heuristics.

### Assumptions

- **No backward compatibility**: old exec cache entries, fingerprints, and approval shapes are discarded. The cache index is rebuilt from scratch.
- **Artifacts are immutable**: once created, an artifact never changes. If content changes, it becomes a new artifact with a new ID. This makes `artifact_id` a stable component of the approval fingerprint.

---

## Core Concepts

### Concept 1: Artifact-Bound Execution Identity

Approval reuse for transient code should be keyed by the executed artifact, not only by the shell command string.

Proposed identity fields:

- artifact content digest or revision content digest
- entrypoint path
- normalized concrete remote targets
- sandbox profile and mounted dependency layers
- optional argument shape if specific argument classes materially affect safety

### Concept 2: Concrete-Target Reuse

Approval reuse should be allowed when concrete hosts are known, even if analysis also detects `import` or `function_call` patterns.

The current rule is too strict because many legitimate scripts contain both:

- structural signals: `import requests`, `requests.get(...)`
- concrete signals: `https://wttr.in/...`

Reuse should be blocked only when network behavior exists but concrete target coverage is incomplete or unresolved.

### Concept 3: Install-Time Inference vs Runtime Approval

Some signals are useful for capability inference but too weak for runtime approval.

- `import requests` is useful when validating or promoting an agent revision
- `import requests` alone should not create a remote-host approval at runtime

Runtime approval should be driven by concrete remote effect and concrete target extraction.

### Concept 4: Promotion Threshold

An artifact should stop being treated as an opaque bundle once it is clearly intended for repeated use.

Promotion signals include:

- stable script entrypoint
- reusable structured output
- explicit or inferred external host usage
- repeated invocation within a workflow
- planner intent that the result is a durable capability, not just a test artifact

---

## Target Architecture

### Path A: Transient Artifact Execution

Introduce a first-class execution path for artifact entrypoints.

Preferred shape:

- new tool such as `artifact.exec` or `script.exec`
- inputs: `artifact_id`, `entrypoint`, `args`, optional env/layers/cwd
- remote-access analysis runs against artifact files and reachable local modules
- approval reuse is bound to artifact identity and concrete targets

This path is for:

- smoke tests
- validation after build
- ad hoc user-triggered runs
- short-lived workflows that do not justify revision creation

### Path B: Durable Script-Agent Execution

Use the existing revision lifecycle:

`artifact.build` → `agent.revision.create` or `agent.revision.create_from_intent` → `agent.revision.promote` → spawn via `execution_mode: script`

This path is for:

- reusable tools
- repeated workflow reuse
- stable external integrations
- capabilities that should carry declared `NetworkAccess` rather than rediscovering hosts from command text on every run

---

## Phase 1: Refine Runtime Remote-Access Semantics

**Goal:** separate weak capability signals from runtime approval-driving signals.

**Primary file:** `autonoetic-gateway/src/runtime/remote_access.rs`

### Task 1.1: Classify patterns by purpose

- [x] Separate pattern roles into:
  - capability inference (`import`, `function_call`)
  - concrete target extraction (`url_literal`, `ip_address`)
  - runtime approval hints (`network_command`, `dependency_install`)
- [x] Make bare imports contribute to capability inference without automatically producing runtime remote-host approvals
- [x] Preserve concrete detection for URL literals and IP addresses

### Task 1.2: Preserve transitive local-module analysis

- [x] Ensure artifact or workspace-backed analysis follows local imports to reachable files (already exists via `analyze_code_with_workspace`)
- [x] Continue extracting concrete targets from imported local modules that embed URLs
- [x] Add tests for module-imported concrete hosts

### Task 1.3: Introduce unresolved-network classification

- [x] Add `NetworkCoverage` enum: `Concrete { targets }`, `Unresolved`, `None`
- [x] `classify_network_coverage()` function classifies patterns using concrete-target presence and dependency-install signals
- [x] Used downstream in sandbox.rs and artifact_exec.rs approval reuse

---

## Phase 2: Refactor Approval Reuse Around Concrete Coverage

**Goal:** allow reuse for normal scripts that mix imports, function calls, and concrete URLs.

**Primary files:**

- `autonoetic-gateway/src/runtime/approved_exec_cache.rs`
- `autonoetic-gateway/src/runtime/tools/sandbox.rs`

### Task 2.1: Replace all-or-nothing `has_concrete_targets` logic

- [x] Replace the current boolean gate with `NetworkCoverage` enum:
  - `Concrete { targets: Vec<String> }` — concrete hosts present, no unresolved signals
  - `Unresolved` — network behavior present but no stable concrete host coverage (or `dependency_install` present)
  - `None` — no network behavior detected
- [x] Classification rule:
  - `import` and `function_call` are capability-inference signals that do **not** block reuse when concrete targets exist
  - `dependency_install` blocks reuse because package installation targets unknown registry hosts
  - `network_command` does not block reuse when concrete targets coexist (the concrete URL covers the target)
- [x] Allow cache, approved-requests, and session-grant reuse when `Concrete`
- [x] Skip all reuse paths when `Unresolved`
- [x] Move session-grants check inside the `Concrete` branch (currently outside the `has_concrete` gate)

### Task 2.2: Stabilize approval identity

- [x] New fingerprint: `SHA256(agent_id | sorted_targets | identity)`
  - When `artifact_id` is present: `identity = "artifact:" + artifact_id` (stable across shell wrappers)
  - When absent: `identity = "code:" + code_to_analyze` (same as before)
- [x] Artifacts are immutable — same `artifact_id` always maps to same content, so the fingerprint is stable
- [x] No backward compatibility — old cache entries are discarded

### Task 2.3: Thread approved target coverage into sandbox network enablement

- [x] Session grants check moved inside the `Concrete` coverage branch — no longer checked for `Unresolved` patterns
- [x] All reuse paths (cache, approved-requests, session grants) now gated by `NetworkCoverage::Concrete`
- [x] `share_net` activates when `approval_validated_for_command = true` from any reuse path

---

## Phase 3: Introduce Artifact-Aware Transient Execution

**Goal:** stop forcing transient artifact runs through opaque shell-string analysis.

**Likely files:**

- `autonoetic-gateway/src/runtime/tools/` (new tool module)
- `autonoetic-gateway/src/runtime/tools/mod.rs`
- `autonoetic-gateway/src/runtime/tools/sandbox.rs`
- `autonoetic-gateway/src/execution.rs` or related execution orchestration

### Task 3.1: Define a first-class transient execution tool

- [x] Add `artifact.exec` tool in `autonoetic-gateway/src/runtime/tools/artifact_exec.rs`
- [x] Accept explicit structured inputs: `artifact_id`, `entrypoint`, `args`, `env`, `approval_ref`
- [x] Resolve artifact contents before approval analysis

### Task 3.2: Analyze files, not just command strings

- [x] Perform remote-access analysis on the entrypoint and workspace files via `analyze_code_with_workspace`
- [x] Derive approval requests from analyzed code content rather than the shell wrapper string
- [x] Bind approval reuse to the artifact identity via artifact-id-based fingerprint

### Task 3.3: Keep `sandbox.exec` as the low-level escape hatch

- [x] Preserve existing `sandbox.exec` for generic command execution (unchanged)
- [x] `artifact.exec` uses artifact-bound analysis and fingerprinting; `sandbox.exec` uses command-string analysis
- [x] Both tools share the same dedup infrastructure (exec cache, session grants, approved requests)

---

## Phase 4: Push Reusable Artifacts Toward Script-Agent Promotion

**Goal:** make durable code become a proper execution subject earlier.

**Likely files:**

- planner and builder SKILL files under `agents/`
- `docs/AGENTS.md`
- `docs/agent_routing_and_roles.md`

### Task 4.1: Add planner heuristics for promotion

- [x] Added "Artifact Execution vs Script-Agent Promotion" section to planner SKILL.md
- [x] Defined promotion signals (stable entrypoint, repeated use, network behavior, durable intent)
- [x] When an artifact has a stable entrypoint and explicit external behavior, prefer revision creation and promotion over repeated executor delegation

### Task 4.2: Route durable artifacts through the builder/registration path

- [x] Planner SKILL.md documents the promotion path: `artifact.build → agent.revision.create_from_intent → agent.revision.promote`
- [x] Coder SKILL.md instructs returning install intent payload with `execution_mode: "script"` for script agents
- [x] Promote script agents with declared `NetworkAccess` to eliminate per-command approval churn

### Task 4.3: Keep transient validation lightweight

- [x] Planner SKILL.md lists `artifact.exec` for transient runs (smoke tests, validation before promotion)
- [x] Executor SKILL.md documents `artifact.exec` for artifact-bound transient execution
- [x] Coder SKILL.md documents `artifact.exec` for testing built artifacts
- [x] Promote only when repeated use or durable registration is intended

---

## Phase 5: Verification and Regression Coverage

**Goal:** lock in the intended semantics with scenario-driven tests.

### Task 5.1: Approval reuse regression

- [x] `test_sandbox_exec_import_plus_url_caches` — import + URL pattern reuses cached approval (was previously blocked)
- [x] `test_sandbox_exec_cache_hit_skips_approval` — URL-only cache hit
- [x] `test_sandbox_exec_cache_miss_requires_approval_for_concrete_url` — cache miss requires approval

### Task 5.2: Weak-signal runtime approval regression

- [x] `test_classify_coverage_import_only_is_unresolved` — import-only patterns classify as Unresolved (no approval reuse)
- [x] `test_classify_coverage_unresolved_with_dependency_install` — dependency_install blocks reuse even with concrete targets

### Task 5.3: Mixed-pattern reuse regression

- [x] `test_classify_coverage_import_plus_url_is_concrete` — import + URL → Concrete (reuse allowed)
- [x] `test_classify_coverage_function_call_plus_url_is_concrete` — function_call + URL → Concrete (reuse allowed)
- [x] `test_classify_coverage_concrete_with_network_command` — network_command + URL → Concrete (reuse allowed)
- [x] `test_classify_coverage_unresolved_function_call_no_url` — function_call without URL → Unresolved (reuse blocked)

### Task 5.4: Script-agent lifecycle coverage

- [x] `test_artifact_analysis_detects_concrete_targets` — weather artifact code analyzed for wttr.in
- [x] `test_artifact_coverage_is_concrete_despite_imports` — import + url_literal → Concrete coverage
- [x] `test_artifact_fingerprint_stable_across_shell_wrappers` — same artifact_id produces same fingerprint regardless of shell wrapper or args
- [x] `test_artifact_fingerprint_differs_across_artifacts` — different artifact_ids produce different fingerprints
- [x] `test_lifecycle_cache_reuse_simulated` — first run records cache, second run hits cache via artifact-id-based fingerprint
- [x] `test_artifact_exec_tool_registered_and_gated` — artifact.exec registered, gated by CodeExecution capability

---

## Execution Order

1. ~~Refine runtime remote-access semantics~~ **Done**
2. ~~Refactor approval reuse and sandbox network enablement~~ **Done**
3. ~~Add artifact-aware transient execution~~ **Done**
4. Update planner/builder routing toward script-agent promotion *(guidance, not code)*
5. ~~Add regression coverage~~ **Done** (Task 5.4 script-agent lifecycle deferred)

---

## Recommended Outcome

The idiomatic model should be:

1. **Transient artifact execution** as a first-class path for testing and ad hoc use
2. **Script-agent promotion** for reusable capabilities

The runtime should **not** make the generic executor looser in a broad way. Instead, it should make transient execution more artifact-aware and make durable execution more explicitly agent-centered.