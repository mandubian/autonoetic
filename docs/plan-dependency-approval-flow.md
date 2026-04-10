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

### Phase 1: Mechanical redirect for non-builder agents

**Priority: Highest.** Stops evaluator/coder dep-install loops with minimal code.

**File:** `autonoetic-gateway/src/runtime/tools/sandbox.rs`

After remote access analysis (around line 830), before the approval check at line 862:

1. Detect if the command contains `network_command` category patterns that are package-manager installs (`pip install`, `npm install`, etc.).
2. Check if the agent has `NetworkAccess` capability (already computed at line 833).
3. If the agent does NOT have `NetworkAccess` AND the detected patterns are exclusively package-manager commands:
   ```rust
   return Ok(json!({
       "ok": false,
       "dependency_layer_required": true,
       "recommended_agent": "packager.default",
       "reason": "External packages must be resolved into layers by packager.default before execution.",
       "detected_patterns": [list of detected pip/npm/etc patterns],
   }).to_string());
   ```

This is ~30 lines. No new types, no new approval actions, no schema changes.

**Files to modify:**
- `autonoetic-gateway/src/runtime/tools/sandbox.rs` — redirect logic
- `agents/specialists/coder.default/SKILL.md` — instruct to expect `dependency_layer_required` and signal planner
- `agents/specialists/evaluator.default/SKILL.md` — same
- `agents/lead/planner.default/SKILL.md` — when coder's artifact has dependency files (requirements.txt, package.json), insert packager step before evaluator

### Phase 2: Layer auto-mounting for evaluator

**Priority: High.** Evaluator runs with builder's dependency layers without LLM assistance.

**Current state:** Evaluator CAN mount layers by passing `artifact_id` in `sandbox.exec` (SandboxExecArgs already has this field, line 559 of tools/mod.rs). The existing code at sandbox.rs:1303-1334 auto-extracts and mounts artifact layers. But the LLM has to know the artifact_id and pass it manually.

**Approach:** Add `artifact_id` to `AgentExecutor` so it's mechanically available to all `sandbox.exec` calls in the child session.

**Files to modify:**

1. **`autonoetic-gateway/src/runtime/tools/agent.rs`** — Add `artifact_id: Option<String>` to `SpawnAgentArgs` JSON schema. When present, thread it through to `spawn_agent_once()`.

2. **`autonoetic-gateway/src/execution.rs`** — Add `artifact_id: Option<String>` parameter to `spawn_agent_once()`. Store on `AgentExecutor` via `.with_artifact_id()` builder method.

3. **`autonoetic-gateway/src/runtime/lifecycle.rs`** — Add `artifact_id: Option<String>` field to `AgentExecutor` (alongside existing `session_id`, `workflow_id`, etc.). Pass it to `NativeToolRunContext`.

4. **`autonoetic-gateway/src/runtime/active_execution_registry.rs`** — Add `artifact_id: Option<String>` to `NativeToolRunContext`.

5. **`autonoetic-gateway/src/runtime/tools/sandbox.rs`** — In the mount construction section (around line 1270): when `NativeToolRunContext` has `artifact_id` AND the current `sandbox.exec` call doesn't already have one in its args, auto-mount the artifact's layers using the existing logic at lines 1303-1334.

6. **`autonoetic-gateway/src/sandbox.rs`** — Add `readonly: bool` to `SandboxMount`. In `bubblewrap_shell_command()`, use `--ro-bind` when `readonly` is true, `--bind` when false. Layer mounts from auto-mounting should set `readonly: true`.

**Key invariant:** Auto-mounted layers are read-only. The evaluator inspects and runs against them but cannot mutate them.

### Phase 3: Safe local inspection commands

**Priority: Medium.** Agents can inspect their environment without triggering approval.

**File:** `autonoetic-gateway/src/runtime/remote_access.rs`

Add a method `is_safe_inspection_command(command: &str) -> bool` that returns true for:
- `pip list`, `pip show <pkg>`, `pip --version`
- `npm list`, `npm version`
- `python3 -c "import pkg; ..."` where the inline code has no socket/connect/requests patterns

**File:** `autonoetic-gateway/src/runtime/tools/sandbox.rs`

Before the approval check (line 862): if `is_safe_inspection_command()` returns true AND the remote analysis found ONLY `network_command` patterns (no `url_literal`, `ip_address`, `import`, `function_call`), set `approval_validated_for_command = true` but keep `share_net = false`. These commands don't need network; they just inspect the local package index.

### Phase 4: Agent rename (builder → packager)

**Priority: Medium.** Should happen before or alongside Phase 1 to avoid updating references twice.

**Steps:**
1. `mv agents/specialists/builder.default agents/specialists/packager.default`
2. Update SKILL.md: `name: "packager.default"`, `agent.id: "packager.default"`
3. Update `config/config-template.yaml`: `llm_preset_mapping: packager: ...` (was `builder`)
4. Update `docs/AGENTS.md` — role table and delegation ladder
5. Update all doc references (see grep results — ~100+ references across docs/)
6. Update test fixtures referencing `builder.default`
7. Update planner, coder, evaluator SKILL.md references to `builder.default` → `packager.default`

### Phase 5: Tests

**New files:**

1. **`autonoetic-gateway/tests/dependency_redirect_integration.rs`**
   - Non-builder agent runs `pip install requests` → gets `dependency_layer_required: true`
   - Builder/packager agent runs `pip install requests` → proceeds normally (has NetworkAccess)

2. **`autonoetic-gateway/tests/layer_auto_mount_integration.rs`**
   - Planner spawns evaluator with `artifact_id` → evaluator's `sandbox.exec` auto-mounts artifact layers
   - Layers are read-only in the sandbox

3. **`autonoetic-gateway/tests/safe_inspection_integration.rs`**
   - `pip list` runs without approval (no network needed)
   - `pip install` still requires approval for non-builder agents

### Phase 6: Documentation

**Files to update:**
- `docs/approval-system.md` — Add "Dependency Install Redirect" section
- `docs/remote-access-approval.md` — Describe packager-owned dependency resolution path
- `docs/ARCHITECTURE.md` — Update agent role table (builder → packager)
- `docs/AGENTS.md` — Update role table and delegation ladder

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
