# Sandbox drivers

How a sandbox backend is selected, what a backend is responsible for, and how to
add one. Implements #1117 (umbrella #1116, modularity & pluggability).

## The seam

Per-agent *selection* was already data-driven — `runtime.sandbox` in a SKILL.md
manifest names the driver, and `SandboxDriverKind::parse` resolves it. The
*implementation set* was not: `SandboxDriverKind` was an enum matched at seven
sites inside `sandbox.rs`, so a new backend meant editing every one of them.

| Behavior | Before | Now |
|---|---|---|
| command construction | `match driver` ×2 (the two spawn paths) | `SandboxDriver::build_command` |
| SDK socket in-sandbox path | `sdk_socket_sandbox_path` match | `SandboxDriver::sdk_socket_path` |
| SDK bridge plumbing (mounts / `-v` / `-e`) | `wire_sdk_bridge` match | `SandboxDriver::wire_sdk_bridge` |
| child process env | `apply_child_env` match | `SandboxDriver::apply_child_env` |
| network-isolation guarantee | `guarantees_network_off` match | `SandboxDriver::guarantees_network_off` |
| dependency-plan support | inline `driver == MicroVm` check | `SandboxDriver::check_dependency_support` |
| process vs in-process tier | inline `driver == Wasm` checks | `SandboxDriver::tier` |

`sandbox.rs` keeps everything *common* to an execution — the SDK bridge socket
and its dispatch, dependency composition, spawning and waiting on the child —
and no longer matches on which driver is in play. The two former spawn paths
(`spawn_with_driver_and_dependencies_and_env` and
`spawn_with_session_content_and_env`) collapsed into one `spawn_process`; they
differed only in the extra mount list.

## Layout

```
autonoetic-gateway/src/
  sandbox.rs                  orchestration + SDK bridge (driver-agnostic)
  sandbox/driver/mod.rs       SandboxDriver trait, SandboxDriverKind, registry
  sandbox/driver/bubblewrap.rs  default backend + host-fs deny-list masking
  sandbox/driver/docker.rs      container backend
  sandbox/driver/microvm.rs     firecracker backend
  sandbox/driver/wasm.rs        in-process WASI tier
```

## Tiers

Two tiers exist, distinguished by `SandboxDriver::tier()`:

- **`DriverTier::Process`** — the driver returns a host `(program, argv)` and
  the runner spawns and waits on a child. bubblewrap, docker, microvm.
- **`DriverTier::InProcess`** — the driver runs the request inside the gateway
  process and returns `ExecOutput` directly. Only the wasm tier today.

Callers that need to build a different request shape for the in-process tier ask
`SandboxDriverKind::runs_in_process()` rather than comparing against `Wasm`, so
a second in-process backend would not need those call sites touched.
`SandboxRunner::run_to_output` is the unified entry that dispatches by tier;
`SandboxRunner::spawn_*` is the process-only path, and it refuses an in-process
driver with a message pointing at `run_to_output`.

## Network guarantees

`guarantees_network_off` is the single source of truth for "is this execution
physically offline", consumed by the promotion gate (P-3.10) to decide whether a
deterministic test suite can be trusted to run in isolation instead of being
pre-denied on import detection. Each driver answers for itself:

| Driver | Guaranteed offline? | Why |
|---|---|---|
| bubblewrap | only under `force_network_off` | `--unshare-all` with no `--share-net`; the gate sets the override |
| docker | always | `docker run --network none` is hardcoded |
| wasm | always | the WASI preview1 tier exposes no sockets |
| microvm | never | the operator's `--config-file` declares the NIC; the gateway cannot assert its absence |

An unregistered kind answers `false`, so the gate fails closed. This is a
*detection* guarantee about the execution tier, distinct from the per-exec
network **grant** decided in `runtime/network_grant.rs` — see
`docs/internals/sandbox/network-grant.md`.

## Adding a driver

1. **One new file** under `sandbox/driver/` implementing `SandboxDriver`.
   Override only what your backend actually does — the trait defaults describe a
   process driver that runs no SDK bridge, inherits no env, accepts dependency
   plans, and makes no network guarantee.
2. **One `SandboxDriverKind` variant.** The kind stays a closed enum because it
   is public API: manifests parse into it and callers hold it.
3. **One line in `builtin_registry()`.**

Manifest compatibility comes from `SandboxDriver::names()` — the registry
resolves `runtime.sandbox` case-insensitively against each driver's declared
names and aliases, so aliases live with their driver instead of in a central
match.

Two tests keep the wiring honest: `every_kind_is_registered` fails if a variant
is added without a registry entry (rather than failing at runtime mid-execution),
and `every_declared_name_resolves_to_its_driver` round-trips every declared name
and alias.

Build-feature gating stays per-driver. The wasm driver is *always* registered so
`sandbox: "wasm"` resolves on every build; only its execution body sits behind
the `wasm-tier` feature, which turns a missing feature into a clear
build-feature error instead of an unknown-driver one.

## What is deliberately not here

- **Dynamic loading.** The driver set is still compile-time; the registry is a
  static list, not a plugin loader. Out of scope per #1116's non-goals.
- **Operator-selectable driver set.** Which drivers exist is not config-driven.
  Per-agent selection already is (`runtime.sandbox`).
