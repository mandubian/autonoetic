# Dependency Approval Flow

## Overview

Reduce dependency-install approval churn by mechanically redirecting non-builder agents, auto-mounting builder's dependency layers into evaluator sessions, and whitelisting safe inspection commands.

## Current Problems

1. **Wrong agents install dependencies.** Coder/evaluator attempts `pip install` and hit repeated approval prompts or loop on failures. Builder already has `NetworkAccess` and auto-approves (sandbox.rs:833-849), but nothing stops other agents from trying.

2. **No mechanical redirect.** When coder/evaluator tries `pip install`, the gateway runs the full remote-access approval flow instead of telling the agent to stop and route through builder.

3. **Evaluator can't access builder's layers automatically.** Builder captures dependency layers via `capture_paths` and embeds them in artifacts. But when planner spawns evaluator, the evaluator's `sandbox.exec` calls don't auto-mount those layers unless the LLM manually passes `artifact_id` in every call.

4. **Safe inspection commands trigger approval.** `pip list`, `pip show`, `python3 -c "import pkg"` are read-only operations that don't need network access, but the remote access analyzer flags them.

5. **Naming confusion between builder agents.** `builder.default` (specialists/) resolves build-time dependencies into layers. `specialized_builder.default` (evolution/) installs new durable agents. The names suggest overlapping purposes but they do fundamentally different things.

## Agent Renaming

### Current names

| Directory | Agent ID | Purpose |
|-----------|----------|---------|
| `agents/specialists/builder.default/` | `builder.default` | Resolves build-time *dependencies* into artifact layers (pip install, layer capture) |
| `agents/evolution/specialized_builder.default/` | `specialized_builder.default` | Installs new *agents* from specifications (revision create, promote) |

### Proposed names

| Directory | Agent ID | Purpose |
|-----------|----------|---------|
| `agents/specialists/packager.default/` | `packager.default` | Resolves build-time *dependencies* into artifact layers |
| `agents/evolution/specialized_builder.default/` | `specialized_builder.default` | Installs new *agents* from specifications (unchanged) |

**Rationale:** "Packager" clearly communicates "I package dependencies into layers." "Specialized builder" clearly communicates "I build/install new agent types." No overlap in the mental model.

**Scope of rename:** Directory rename + SKILL.md `name`/`agent.id` fields + `llm_preset_mapping` keys in config + all doc references + all test fixtures that reference `builder.default`.

## Implementation Phases

### Phase 1: Mechanical redirect for non-builder agents ✅ DONE

**Priority: Highest.** Stops evaluator/coder dep-install loops with minimal code.

Implemented in commit `1839f66`.

### Phase 2: Layer auto-mounting for evaluator ✅ DONE

**Priority: High.** Evaluator runs with builder's dependency layers without LLM assistance.

Implemented in commit `eb142e1`.

### Phase 3: Safe local inspection commands ✅ DONE

**Priority: Medium.** Agents can inspect their environment without triggering approval.

Implemented in commit `60ec0a3`.

### Phase 4: Agent rename (builder → packager) ✅ DONE

**Priority: Medium.** Done before Phase 1 to avoid updating references twice.

Implemented in commit `ccf019a`.

### Phase 5: Tests ✅ DONE

Integration tests in `autonoetic-gateway/tests/dependency_redirect_integration.rs`.

Implemented in commit `905160e`.

### Phase 6: Documentation updates

**Priority: Low.** In progress.

## Pluggability and Evolution

### The plan is already runtime-agnostic

The redirect logic (Phase 1) does NOT hardcode `pip install`. It operates on the existing `network_command` detection category in `remote_access.rs:252-354`, which already covers all runtimes:

| Runtime | Commands detected |
|---------|------------------|
| Python | `pip install`, `pip3 install` |
| Node | `npm install`, `yarn install`, `yarn add`, `pnpm install`, `bun install` |
| Go | `go get`, `go mod download` |
| Rust | `cargo install` |
| Ruby | `gem install` |
| PHP | `composer install`, `composer require` |
| System | `apt-get install`, `apk add`, `yum install`, `dnf install`, `pacman -S` |
| VCS | `git clone`, `git fetch`, `git pull`, `git push` |
| Download | `curl`, `wget` |

Adding a new runtime means adding one line to the `command_patterns` table in `remote_access.rs`. The redirect, approval bypass, and safe-inspection logic all work automatically because they're category-based, not command-specific.

### Capability-based, not agent-name-based

The redirect checks for the `NetworkAccess` capability, not `agent_id == "packager.default"`. Any agent with `NetworkAccess` in its manifest auto-approves dependency installs. Any agent without it gets the redirect. This means:
- Adding a new packager agent (e.g., `packager.rust`) with `NetworkAccess` works without gateway changes
- Removing `NetworkAccess` from an agent automatically gates it

### Safe inspection is data-driven

Phase 3's allowlist should be implemented as a static table, not inline conditionals:

```rust
const SAFE_INSPECTION_COMMANDS: &[&str] = &[
    "pip list", "pip show ", "pip --version",
    "npm list", "npm version",
    // Future: "cargo --version", "go version", etc.
];
```

Adding new safe commands is a one-line table entry.

### Future evolution paths the plan opens

1. **`DependencyInstall` approval action type.** If we later want to tighten packager's blanket `NetworkAccess`, we add a `ScheduledAction::DependencyInstall` variant. The redirect logic becomes: "if agent has `DependencyResolution` capability AND approval exists for this (runtime, package_set_digest), auto-approve." The current Phase 1 redirect is the foundation — it already identifies the category, just needs a second grant-check path.

2. **Approval provider interface.** The approval system currently resolves approvals via SQLite + operator CLI. The `ScheduledAction` enum + `ApprovalRequest` struct form a natural provider boundary. A future `ApprovalProvider` trait could swap in:
   - Policy-as-code (auto-approve based on org rules)
   - External approval services (e.g., Slack-based approval)
   - Signed approval bundles (offline/pre-approved scopes)

   The gateway's approval logic is already centralized in `scheduler/approval.rs` and `scheduler/decision.rs` — it's one indirection away from being provider-based.

3. **Artifact-level dependency declarations.** `dependency_plan_from_args_or_lock` (tools/mod.rs:573) already normalizes dependency intent from either explicit args or `runtime.lock`. A future step could allow artifacts to declare their dependency requirements in their manifest, enabling the gateway to automatically route artifacts through packager without planner guidance.

4. **Layer composition.** `artifact.build` already accepts `layers: Vec<ArtifactLayer>`. Multiple packager runs (Python deps + Node deps) could produce separate layers composed into one artifact. No gateway changes needed — the layer infrastructure already supports this.

### What keeps the gateway dumb

Each evolution path extends the gateway by adding **new data tables** (commands, capabilities, action types) or **new provider boundaries** (approval provider), not by adding judgment logic. The gateway's role remains:
- Match patterns against static tables (deterministic)
- Check capabilities against manifests (mechanical)
- Enforce policy: approve, redirect, or deny (rule-based)

The intelligence lives in the planner's routing decisions and the agents' behavior — the gateway just enforces the rules they operate under.

## What We're NOT Building

| Rejected approach | Why |
|---|---|
| `dependency.resolve` as a separate tool | Builder/packager already has `NetworkAccess` and auto-approves. The churn is from non-builder agents, which the redirect solves. |
| `DependencyInstall` approval action type | Not needed until we want to tighten builder's blanket `NetworkAccess`. Adds complexity without solving the immediate pain. |
| `package_set_digest` normalization | Over-engineering for now. The immediate problem is looping, not approval dedup. |
| TTL-based approval expiry | Session-scoped grants (already built) are sufficient. TTL adds clock-skew complexity for no benefit. |

## Existing Infrastructure We're Leveraging

| Component | Location | What it provides |
|-----------|----------|-----------------|
| `SandboxMount` | `sandbox.rs:696` | Bind mount struct — adding `readonly` field |
| `LayerStore` | `layer_store.rs` | Content-addressed layer storage (compress, extract) |
| `ArtifactLayer` | `types/layer.rs:28` | Layer references in artifact bundles |
| `SandboxExecArgs.artifact_id` | `tools/mod.rs:559` | Already exists — triggers layer mounting in sandbox.exec |
| Layer auto-mount in sandbox.exec | `sandbox.rs:1303-1334` | Extracts and mounts artifact layers — reuse this logic |
| `NetworkAccess` auto-approve | `sandbox.rs:833-849` | Builder already bypasses approval for all remote access |
| `capture_paths` | `sandbox.rs:1418-1487` | Captures sandbox directories as layers after execution |
| `artifact.build` with `layers` | `tools/artifact.rs` | Embeds layer references into artifact manifests |
| `LockedLayerMount` | `types/runtime_lock.rs:49` | Pinned layer mounts in runtime.lock |
| `dependency_plan_from_args_or_lock` | `tools/mod.rs:573` | Normalizes dependency intent from args or runtime.lock |

## Estimated Scope

| Phase | Files changed | Lines | Risk |
|-------|---------------|-------|------|
| 1. Mechanical redirect | ~5 | ~50 | Low |
| 2. Layer auto-mounting | ~6 | ~200 | Medium |
| 3. Safe inspection | ~2 | ~50 | Low |
| 4. Agent rename | ~30+ | rename-only | Low (mechanical) |
| 5. Tests | ~3 new | ~300 | Low |
| 6. Docs | ~5 | ~100 | Low |
| **Total** | ~50 | ~700 | — |

## Dependency on Prior Work

This plan builds on the session approval grants and loop guard work already merged:
- Session approval grants (migration v4) — host-level auto-approval within root session
- Loop guard fingerprinting — prevents repeated identical tool calls
- Promotion severity gating — prevents broken artifacts from being promoted
- The mechanical redirect in Phase 1 is complementary to the loop guard: redirect stops the loop *before* it starts, loop guard catches it if the redirect is missed.
