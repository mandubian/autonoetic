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

## Host-filesystem exposure (`host_fs`)

What of the *host* exists inside the sandbox is a per-driver property, and only
bubblewrap has a choice about it. `config:sandbox.host_fs` selects between two
modes; docker, microvm and wasm already behave like `allow_set` because none of
them binds the host root, so the key changes the bubblewrap tier only.

| Mode | Behaviour |
|---|---|
| `allow_set` (default, DP-1) | Nothing of the host exists except what the gateway asserts |
| `legacy` (deprecated opt-out) | `--ro-bind / /` — the whole host filesystem is readable inside the sandbox. A warning is logged at startup, and the secret mask is the stopgap that keeps credentials out |

The mode is a property of the **tier**, not of one tool: `sandbox_exec`,
script-mode agents, `artifact_exec` and the promotion gate all resolve it
through `host_fs_allow_set` in `sandbox.rs`. A path that constructed
`BwrapIsolationOverrides` without asking would keep the host root bound while
the startup log and this table said otherwise — which is why the resolution
lives in one function rather than at each call site.

### Mount destinations

A mount is only as good as its destination: bwrap `mkdir`s the target inside
the namespace, and a target it cannot create kills the sandbox at setup —
before *any* command, including `echo ok`. `mount_destination_flags` decides
this mechanically, and the answer turns on whether the destination already
exists:

| Destination | Outcome |
|---|---|
| Under the workspace (`/tmp`) | Nothing needed — the workspace bind is writable |
| Already exists | Nothing needed — bwrap binds *over* an existing entry without `mkdir`, including inside a read-only bind |
| Missing, inside a toolchain/name-resolution bind | Refused: `mkdir` in a read-only bind cannot work |
| Missing, `allow_set` | Nothing needed — the root is an empty writable tmpfs |
| Missing, `legacy` | The deepest **existing** ancestor is replaced by a `--tmpfs` and its entries re-bound read-only, but only for `/opt`, `/mnt`, `/srv`, `/media`. Anything else — `/etc`, `/var`, `/` — is refused and names the flip |

Two refusals are about honesty rather than capability: a destination that
overlaps another mount of the same exec, and a `legacy` tmpfs that would sit
on top of an earlier mount. Neither fails in bwrap — the later mount silently
hides the earlier layer, which is worse than a refusal that says so.

### What the allow-set contains

`allow_set` cannot be expressed as "bind less", because bwrap's default root
*is* the host root. Composition therefore starts by making the root empty and
layers upward, in order:

1. `--tmpfs /` — an empty root, so every later bind is additive rather than
   subtractive. This is the step that makes the mode a whitelist.
2. `--proc /proc`.
3. **Toolchain roots**, read-only: `/usr`, `/lib`, `/lib64`, `/bin`, `/sbin`,
   `/etc/ld.so.cache`. Candidates absent on this host are skipped. Symlinked
   paths are canonicalized as the bind *source* but bound at their **original**
   path, so on merged-`/usr` systems a command referencing either `/bin/sh` or
   `/usr/bin/sh` resolves.
4. **Name resolution**, read-only: `/etc/resolv.conf`, `/etc/hosts`,
   `/etc/nsswitch.conf` — needed whenever the exec may reach the network, and
   harmless (three tiny file binds) when it may not.
5. **The Python SDK tree**, read-only, bound *at its own path* rather than by
   exposing the gateway directory — that distinction is what keeps gateway
   secrets out of the sandbox. Without this bind `import autonoetic_sdk` fails
   under `allow_set`, because the legacy blanket bind used to supply it.
6. **Workspace, layers, declared and granted mounts, session content.**

`ALLOW_SET_TOOLCHAIN_ROOTS` and `ALLOW_SET_NAME_RESOLUTION` in
`sandbox/driver/bubblewrap.rs` are the literal lists; `host_fs_mode()` resolves
the mode from the per-exec overrides.

### Declared mounts are the filesystem's `allowed_hosts`

Reach beyond the allow-set is **declared by the agent and asserted by the
gateway**, the same shape as network egress: a `SKILL.md` `runtime.mounts`
entry asks, the operator's config bounds what may be granted, and the gateway
decides per exec.

- `config:sandbox.allowed_mount_roots` — roots any declared mount must sit
  under. A mount is granted iff its canonicalized path is under one of them.
- `config:sandbox.allowed_mount_roots_rw` — the subset that may be granted
  read-write. A declared mount may **narrow** (ro under an rw root) but never
  **widen**: rw under a read-only root is refused.

Refusals name the config key that would permit the mount rather than only
stating that it was denied, so a denial teaches the fix — the same principle as
the network-grant refusals. Enforcement lives in `sandbox.rs`; the manifest-side
surface is in `runtime/tools/sandbox.rs`.

Undeclared reach is a decision, not an oversight: under `allow_set` a path
nobody declared is simply absent, and the exec fails on a missing file rather
than silently reading something it was never granted.

## Whose capabilities apply when an evaluator runs an artifact

An evaluator executing an artifact passes the **target agent's** manifest
capabilities, not its own. That looks like privilege inheritance and is not: the
evaluator's job is to test what the artifact does, the artifact's capabilities
were already reviewed at install time by `agent.install` (which gates the
high-risk ones, `NetworkAccess` included), and without this rule a
network-dependent agent could never be evaluated at all.

The direction matters. This is not the evaluator gaining reach; it is the
artifact's already-approved reach being applied to the run that tests it.

For `sandbox_exec` this sits *under* the per-exec grant decision — a capability
is a ceiling, never itself the grant. See
[`network-grant.md`](network-grant.md), which also lists the sibling exec paths
where capability-as-grant remains the deliberate policy.

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
