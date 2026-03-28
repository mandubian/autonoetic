# Spec: Capability-Driven Sandbox Isolation

**Date:** 2026-03-28
**Status:** Draft

---

## Problem

Sandbox network isolation (`--unshare-all`) is a **global** setting controlled by `GatewayConfig.sandbox.share_net` or `AUTONOETIC_BWRAP_SHARE_NET`. It cannot vary per agent or per execution. This causes:

1. **Evaluator cannot test network-dependent artifacts** — weather-fetcher smoke tests return `NETWORK_ERROR` because the evaluator's sandbox has no network, even though the artifact was approved with `NetworkAccess` capability.
2. **Installed agents cannot use their approved capabilities** — `weather-fetcher` was approved with `NetworkAccess(hosts: [api.open-meteo.com])` but its runtime sandbox still runs `--unshare-all`.
3. **Builder.default `sandbox.conf` is dead code** — nothing reads per-agent sandbox config files.

Additionally, the planner's agent creation flow (Steps 1-7) does not include `builder.default`, so artifacts with `requirements.txt` never get their dependencies layered.

## Solution

### 1. Capability-Driven Sandbox Isolation

When `sandbox.exec` or script execution runs, check the agent manifest's capabilities. If `NetworkAccess` is declared and approved, inject `--share-net` into the bwrap command for that execution only.

**Mechanism:**
- `append_bwrap_isolation_flags` gains an optional `overrides: &BwrapIsolationOverrides` parameter
- A new function `isolation_overrides_from_manifest(manifest: &AgentManifest) -> BwrapIsolationOverrides` derives overrides from the agent's capabilities
- The caller (`sandbox.exec` tool, `execute_script_in_sandbox`) passes the overrides from the active manifest
- Global config remains the default; per-execution overrides are additive

**`BwrapIsolationOverrides`:**
```rust
pub struct BwrapIsolationOverrides {
    pub share_net: bool,
}
```

Derived by:
- `share_net = true` if manifest has any `Capability::NetworkAccess { .. }`

### 2. Fix Planner Agent Creation Flow

Add an explicit **Step 2a** to the planner's agent creation flow between coder and evaluator:

```
Step 2a: If artifact contains dependency files (requirements.txt, package.json, etc.),
         delegate to builder.default to create a layered artifact before evaluation.
```

This makes builder invocation deterministic rather than relying on the LLM noticing the decision flow table entry.

### 3. Cleanup

- Remove dead `agents/specialists/builder.default/sandbox.conf` (not read by anything)
- The global `share_net` config and env var remain as fallback/admin override

## What Changes

### `autonoetic-gateway/src/sandbox.rs`
- Add `BwrapIsolationOverrides` struct
- Add `isolation_overrides_from_manifest()`
- Change `append_bwrap_isolation_flags(argv, overrides)` to accept overrides
- Update all callers: `bubblewrap_command`, `bubblewrap_shell_command`
- When `overrides.share_net == true`, append `--share-net` after `--unshare-all`

### `autonoetic-gateway/src/runtime/tools.rs` (`sandbox.exec`)
- Compute overrides from `manifest` (already available as parameter)
- Pass overrides through to sandbox spawn

### `autonoetic-gateway/src/execution.rs` (`execute_script_in_sandbox`)
- Accept `&AgentManifest` parameter (or just capabilities)
- Compute overrides from manifest capabilities
- Pass overrides to `bubblewrap_command`

### `agents/lead/planner.default/SKILL.md`
- Add explicit Step 2a between coder and evaluator for dependency layering
- Make builder.default a required step when requirements.txt/package.json exists

### Cleanup
- Delete `agents/specialists/builder.default/sandbox.conf`

## What Does NOT Change

- Global `GatewayConfig.sandbox.share_net` still works as default
- `AUTONOETIC_BWRAP_SHARE_NET` env var still overrides globally
- `OnceLock<SandboxConfig>` remains for global defaults
- Agents without `NetworkAccess` still get `--unshare-all` (no network)
- All existing tests continue to pass (backward compatible)

## Security Model

- Only **approved** capabilities affect sandbox isolation
- Manifest capabilities are set at install time via `agent.install`, which requires approval for high-risk capabilities (including `NetworkAccess`)
- The evaluator running `sandbox.exec` with an artifact's manifest inherits that artifact's approved capabilities — this is correct because:
  - The evaluator is testing what the artifact does
  - The artifact's capabilities were already reviewed at install time
  - Without this, evaluation of network-dependent agents is impossible
- For the specific case of evaluator testing an artifact, the evaluator passes the *target agent's* manifest capabilities, not its own
