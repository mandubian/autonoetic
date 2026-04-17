# Plan: Artifact Execution, Approval Reuse, and Script-Agent Promotion

**Date:** 2026-04-17
**Status:** Proposed
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

- [ ] Separate pattern roles into:
  - capability inference
  - concrete target extraction
  - runtime approval hints
- [ ] Make bare imports contribute to capability inference without automatically producing runtime remote-host approvals
- [ ] Preserve concrete detection for URL literals and IP addresses

### Task 1.2: Preserve transitive local-module analysis

- [ ] Ensure artifact or workspace-backed analysis follows local imports to reachable files
- [ ] Continue extracting concrete targets from imported local modules that embed URLs
- [ ] Add tests for module-imported concrete hosts

### Task 1.3: Introduce unresolved-network classification

- [ ] Distinguish between:
  - concrete host coverage present
  - network behavior present but unresolved
  - no network behavior present
- [ ] Use this classification downstream in approval reuse and approval creation

---

## Phase 2: Refactor Approval Reuse Around Concrete Coverage

**Goal:** allow reuse for normal scripts that mix imports, function calls, and concrete URLs.

**Primary files:**

- `autonoetic-gateway/src/runtime/approved_exec_cache.rs`
- `autonoetic-gateway/src/runtime/tools/sandbox.rs`

### Task 2.1: Replace all-or-nothing `has_concrete_targets` logic

- [ ] Replace the current boolean gate with a richer decision structure:
  - `concrete_targets`
  - `has_unresolved_network_intent`
  - `has_runtime_network_behavior`
- [ ] Allow cache and session-grant reuse when concrete targets exist and unresolved network intent is absent
- [ ] Refuse reuse when network intent exists but no stable targets can be derived

### Task 2.2: Stabilize approval identity

- [ ] Define a new fingerprint model for transient executions using:
  - code or artifact identity
  - concrete normalized targets
  - agent or execution subject identity
  - dependency-layer context if relevant
- [ ] Keep approval replay precise enough to avoid over-broad host reuse

### Task 2.3: Thread approved target coverage into sandbox network enablement

- [ ] Ensure reused approvals and session grants drive sandbox network enablement in fresh executions
- [ ] Avoid cases where approval is logically reusable but `share_net` is not applied because the current command surface is too weak
- [ ] Add regression tests for cross-session reuse

---

## Phase 3: Introduce Artifact-Aware Transient Execution

**Goal:** stop forcing transient artifact runs through opaque shell-string analysis.

**Likely files:**

- `autonoetic-gateway/src/runtime/tools/` (new tool module)
- `autonoetic-gateway/src/runtime/tools/mod.rs`
- `autonoetic-gateway/src/runtime/tools/sandbox.rs`
- `autonoetic-gateway/src/execution.rs` or related execution orchestration

### Task 3.1: Define a first-class transient execution tool

- [ ] Add a tool such as `artifact.exec` or `script.exec`
- [ ] Accept explicit structured inputs:
  - `artifact_id`
  - `entrypoint`
  - `args`
  - optional env, cwd, layers, stdin mode
- [ ] Resolve artifact contents before approval analysis

### Task 3.2: Analyze files, not just command strings

- [ ] Perform remote-access analysis on the selected entrypoint and reachable local modules
- [ ] Derive approval requests from analyzed code content rather than the shell wrapper string
- [ ] Bind approval reuse to the artifact execution identity

### Task 3.3: Keep `sandbox.exec` as the low-level escape hatch

- [ ] Preserve existing `sandbox.exec` for generic command execution
- [ ] Document that reusable artifact runs should prefer the new artifact-aware tool
- [ ] Avoid duplicating long-term policy semantics in both paths

---

## Phase 4: Push Reusable Artifacts Toward Script-Agent Promotion

**Goal:** make durable code become a proper execution subject earlier.

**Likely files:**

- planner and builder SKILL files under `agents/`
- `docs/AGENTS.md`
- `docs/agent_routing_and_roles.md`

### Task 4.1: Add planner heuristics for promotion

- [ ] Teach the planner to distinguish between:
  - one-off validation
  - reusable capability creation
- [ ] When an artifact has a stable entrypoint and explicit external behavior, prefer revision creation and promotion over repeated executor delegation

### Task 4.2: Route durable artifacts through the builder/registration path

- [ ] Ensure the planner delegates reusable artifacts to the proper revision-creation flow
- [ ] Prefer `agent.revision.create_from_intent` when the planner has semantic knowledge but the gateway should canonicalize the final manifest
- [ ] Promote script artifacts with declared `NetworkAccess` so runtime policy becomes capability-driven instead of command-shape-driven

### Task 4.3: Keep transient validation lightweight

- [ ] Do not require promotion for every smoke test
- [ ] Use transient artifact execution for initial validation before promotion
- [ ] Promote only when repeated use or durable registration is intended

---

## Phase 5: Verification and Regression Coverage

**Goal:** lock in the intended semantics with scenario-driven tests.

### Task 5.1: Approval reuse regression

- [ ] Add a test where a script artifact with concrete host `wttr.in`:
  - first run triggers approval
  - second run in a fresh child session reuses approval
  - fresh sandbox still receives the correct network enablement

### Task 5.2: Weak-signal runtime approval regression

- [ ] Add a test where `python3 -c "import requests"` does not create a remote-host approval by itself
- [ ] Keep install-time capability inference tests intact so revision validation still detects likely network capability needs

### Task 5.3: Mixed-pattern reuse regression

- [ ] Add a test where `import` + `function_call` + `url_literal` still qualifies for approval reuse when concrete targets are fully covered
- [ ] Add a test where dynamically constructed or unresolved hosts do not qualify for reuse

### Task 5.4: Script-agent lifecycle coverage

- [ ] Add an end-to-end test showing:
  - artifact build
  - revision create/promote
  - script execution through `execution_mode: script`
  - declared `NetworkAccess` eliminates per-command approval churn for the promoted agent

---

## Execution Order

1. Refine runtime remote-access semantics
2. Refactor approval reuse and sandbox network enablement
3. Add artifact-aware transient execution
4. Update planner/builder routing toward script-agent promotion
5. Add regression coverage and documentation updates

---

## Recommended Outcome

The idiomatic model should be:

1. **Transient artifact execution** as a first-class path for testing and ad hoc use
2. **Script-agent promotion** for reusable capabilities

The runtime should **not** make the generic executor looser in a broad way. Instead, it should make transient execution more artifact-aware and make durable execution more explicitly agent-centered.