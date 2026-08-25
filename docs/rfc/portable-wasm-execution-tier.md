# RFC: Portable WASM Execution Tier

- **Status:** Draft (for review)
- **Author:** (proposed by operator; drafted with Claude Code)
- **Scope:** `autonoetic-gateway` sandbox/execution layer, `autonoetic-types` manifest + runtime lock, agent SDK
- **Related:** `docs/guide/cognitive-capsule.md`, `docs/internals/storage/content-store.md`, `docs/ARCHITECTURE.md`, constitution §3 (`docs/constitution/versions/<latest>/constitution.md`)

## 1. Summary

Introduce a second, **portable execution tier** for agents based on WebAssembly
(WASI), alongside the existing OS-process tier (bubblewrap / docker / microvm).
The WASM tier runs **in-process** via an embedded runtime, requires **no external
binary**, works on **all host platforms** (Linux/macOS/Windows), and produces a
**hermetic, content-addressed closure** — making it the natural execution format
for a Cognitive Capsule and the answer to "run safely without bubblewrap" for the
subset of agents that don't need native dependencies or system tools.

This is **not** a replacement for bubblewrap. It is a new tier reached by
**softening the execution contract** so it describes *intent* (run this code,
with these deps, calling these gateway tools) rather than *mechanism* (this shell
line, runtime pip, this Unix socket). The softenings are independently valuable
and are sequenced so each ships and de-risks the next.

## 2. Motivation

- **Onboarding.** Bubblewrap is Linux-only and frequently absent; today the first
  `sandbox.exec` fails with a raw `bwrap: command not found`. A WASM tier needs no
  system dependency.
- **Portability / capsules.** A WASI component *is* a hermetic, portable bundle —
  the logical endpoint of `RuntimeLock` (`autonoetic-types/src/runtime_lock.rs:101`,
  `layers` field "forming the execution closure") and the Cognitive Capsule concept.
- **Stronger boundary.** Host-function imports are capability-scoped; they remove
  the bind-mounted-socket attack surface and tighten Separation of Powers.
- **Better resource control.** wasmtime fuel/epoch + `StoreLimits` give cleaner
  CPU/memory enforcement than process cgroup heuristics (constitution P-3.7).

### Non-goals

- Running the full shipped agent set in WASM (native-dep and system-tool agents
  stay on bubblewrap/docker — see §9 Ceilings).
- Replacing or weakening the bubblewrap tier or any constitutional isolation rule.
- `wasi-sockets`-based agent networking (deferred; see §9).

## 3. Current contract (what we must soften)

The execution layer today (`autonoetic-gateway/src/sandbox.rs`) is uniformly POSIX.
Every driver returns a host `(program, argv)`; **bubblewrap and docker** run the
composed entrypoint via `sh -c "<entrypoint>"`. (MicroVm is the exception: today
`microvm_command` **ignores** the entrypoint and returns
`("firecracker", ["--config-file", <cfg>])` — `sandbox.rs:1223`, param `_entrypoint`
— so the firecracker path is effectively a stub w.r.t. the composed entrypoint.)

```
spawn_with_driver_and_dependencies_and_env(driver, agent_dir, entrypoint,
    dependencies: Option<&DependencyPlan>, overrides, extra_env, root_session_id)
  → match driver {
        Bubblewrap => bubblewrap_shell_command(...),   // sandbox.rs:265  (sh -c)
        Docker     => docker_command(...),             // sandbox.rs:277  (sh -c)
        MicroVm    => microvm_command(...),            // sandbox.rs:278  (ignores entrypoint)
    } → (String /*program*/, Vec<String> /*argv*/)
  → Command::new(program).args(argv).spawn() → SandboxRunner { process: Child, .. }
```

Three pillars block WASM:

| Pillar | Where | WASM problem |
|---|---|---|
| **Arbitrary shell** | `compose_entrypoint` emits `sh -c`, incl. `python3 -m venv && pip install … && <run>` (`sandbox.rs:1234`, `:1270`) | WASM has no shell, no `bash`, no system binaries |
| **Runtime pip/npm** | same `compose_entrypoint`; `DependencyPlan { runtime, packages }` (`sandbox.rs:171`) | No subprocess / native build in WASM |
| **Unix-socket SDK bridge** | `start_sdk_bridge` + bind-mounted socket (`sandbox.rs:252-260`); only started `if driver == Bubblewrap` | WASI has no bind mounts; `wasi-sockets` immature |

Note the SDK bridge dispatch is already pure JSON-RPC: `dispatch_sdk_method(method,
params, agent_dir, gateway_dir, root_session_id)` (`sandbox.rs:727`). The transport
is incidental — that's the lever.

Also note `ExecutionMode::Script` (`autonoetic-types/src/agent.rs:359`) already
expresses "run a declared entry, not free-form shell" — the intent model partly
exists.

## 4. Target architecture: two tiers

```
                    ExecutionRequest (intent)
                            │
            ┌───────────────┴────────────────┐
   SandboxBackend::Process            SandboxBackend::Wasm
   (bwrap / docker / microvm)         (wasmtime, in-process)
   sh -c rendering                    component instantiation
   runtime pip + layers               prebuilt WASI layers only
            │                                  │
            └────────── SdkTransport ──────────┘
              socket (mount)   |   host-function imports
                       (same dispatch_sdk_method)
```

- **Native tier:** arbitrary shell, runtime pip, system tools. Linux/Docker.
- **Portable tier:** language-tagged code, prebuilt WASI deps, host-function SDK,
  no system binaries, no native deps. All platforms, in-process.

### 4.1 Driver support priority & verification matrix

The principles in this RFC (intent-based exec, transport-abstracted SDK, locked
dependency layers, Hermetic capsules, the constitution mappings) must be **proven
on the default sandboxes first**. Priority order:

1. **bubblewrap** — primary, easiest to test locally; the existing baseline.
2. **docker** — co-equal first-class target, verified alongside bubblewrap in
   every early phase. Today docker lags: it has **no SDK bridge**
   (`sandbox.rs:252` gates the bridge to bubblewrap) and is otherwise `--network
   none` + `sh -c` only. Bringing docker to parity is **in-scope for P1–P3**.
3. **wasm** — next (P4+), once the contract is intent-based and layers exist.
4. **firecracker / microvm** — **deferred until after WASM.** It is currently a
   stub (`microvm_command` ignores the entrypoint, `sandbox.rs:1223`); reaching
   parity is real work with low near-term payoff, so it explicitly trails WASM.

**Verification matrix** — every early-phase acceptance criterion is checked on
**both bubblewrap and docker**. Tests are availability-gated (extend the existing
`is_bwrap_available()` pattern with `is_docker_available()`); a driver that isn't
installed skips with a message rather than failing, so a contributor with only
bwrap (or only docker) still gets a green local run, and CI covers both.

| Capability proven per phase | bwrap | docker | wasm | firecracker |
|---|---|---|---|---|
| P1 SDK transport | ✅ verify | ✅ **bring to parity + verify** | — | deferred |
| P2 intent exec | ✅ verify | ✅ verify | — | deferred |
| P3 locked layers + Hermetic round-trip | ✅ verify | ✅ verify | (WASI ABI in P4) | deferred |
| P4 WASM tier | n/a | n/a | ✅ verify | deferred |

## 5. Design

### 5.1 Evolution A — Execution as intent (`ExecutionRequest`)

Replace the bare `entrypoint: &str` with a structured request. New type in
`autonoetic-gateway/src/sandbox.rs` (or a new `sandbox/exec_request.rs`):

```rust
pub enum ExecutionKind {
    /// Legacy/native tier: free-form shell rendered to `sh -c`.
    Shell { command: String },
    /// Portable tier: language-tagged code. Backends choose the interpreter.
    Code { language: CodeLanguage, source: CodeSource, args: Vec<String> },
}

pub enum CodeLanguage { Python, JavaScript /* extensible */ }
pub enum CodeSource { Inline(String), Entry(String) /* path within workspace */ }

pub struct ExecutionRequest {
    pub kind: ExecutionKind,
    pub dependencies: Option<DependencyPlan>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<SandboxMount>,
    pub network: NetworkIntent,        // derived from capabilities, as today
    pub root_session_id: Option<String>,
}
```

- The **Process** backend renders `ExecutionKind::Code{Python,..}` to
  `python3 <entry>` (preserving today's behavior) and `Shell` to `sh -c`.
- The **Wasm** backend accepts **only `ExecutionKind::Code`**; `Shell` returns a
  typed `UnsupportedInTier` error caught at validation time (clear message, not a
  runtime crash).
- The existing `sandbox.exec` tool (`runtime/tools/sandbox.rs:2171`) and
  script-mode path (`runtime/script_execute.rs:176`) construct `ExecutionRequest`
  instead of a string. `ExecutionMode::Script` maps directly to `Code{Entry}`.

### 5.2 Evolution B — Backends behind a trait

```rust
pub trait SandboxBackend {
    /// Validate the request is runnable in this tier (e.g. Wasm rejects Shell,
    /// rejects native deps). Pure, no side effects.
    fn validate(&self, req: &ExecutionRequest) -> Result<(), TierError>;

    /// Run to completion (or spawn), streaming stdout/stderr, returning a handle
    /// that abstracts over an OS Child and an in-process WASM run.
    fn run(&self, req: &ExecutionRequest, sdk: SdkTransport) -> anyhow::Result<Box<dyn ExecHandle>>;
}

pub trait ExecHandle: Send {
    fn wait_with_output(self: Box<Self>) -> anyhow::Result<ExecOutput>; // stdout, stderr, exit_code
    fn kill(&mut self) -> anyhow::Result<()>;
    fn escape_signals(&self) -> &[SandboxEscapeSignal]; // feeds P-7.22 accounting
}
```

`SandboxRunner` becomes a thin wrapper that selects a backend from
`SandboxDriverKind` and holds the `Box<dyn ExecHandle>`. `Process` backends wrap
`std::process::Child`; the `Wasm` backend wraps a `wasmtime` store + instance run
on a worker thread.

### 5.3 Evolution C — SDK transport abstraction

`dispatch_sdk_method` is unchanged. Introduce:

```rust
pub enum SdkTransport {
    /// Bind-mounted Unix socket. Today this is wired for **bubblewrap only**
    /// (`sandbox.rs:252`, `if driver == Bubblewrap`); P1 extends it to
    /// docker/microvm.
    Socket(SocketBridge),
    /// WASM: host-function imports the guest calls in-process.
    HostFunctions(HostFnBridge),
}
```

Both terminate in the **same** `dispatch_sdk_method`. The `HostFnBridge` registers
WASI Component-Model imports (a small WIT interface, e.g. `autonoetic:sdk/host`)
exposing one call: `rpc(method: string, params: json) -> json`, wrapped with the
**P-7.21 rate/size limits** currently in `start_sdk_bridge`. **Side benefit:** this
also lets the socket transport be wired for docker/microvm, closing the current
gap where the bridge is bubblewrap-only (`sandbox.rs:252`).

SDK method families exposed today (must all work over both transports):
`memory.*`, plus the content/knowledge/artifact/execution/digest/user families in
`dispatch_sdk_method` (`sandbox.rs:727+`).

### 5.4 Evolution B′ — Dependencies as pinned, embeddable layers (all tiers)

**This is not WASM-specific.** Embedded, ahead-of-time dependency closures are a
*tier-independent* requirement for immutable, portable, exportable agents — the
whole point of a Cognitive Capsule ("agent bundle plus its runtime closure"). The
machinery already exists; the gap is that **runtime `pip install` is still the
default**, which is exactly what breaks capsule immutability/portability (it needs
network + a resolver at spawn and isn't reproducible). WASM merely *forces* the
issue (it can't `pip install` at all).

What already exists (reuse, don't reinvent):

- **Two dependency representations** in `RuntimeLock` (`runtime_lock.rs`):
  `LockedDependencySet { runtime, packages }` — declarative "install at spawn"
  (the non-portable, runtime-pip path) — and **`LockedLayerMount { layer_id,
  digest, mount_path, approval_scope }`** — a pinned, digest-verified,
  content-addressed, read-only prebuilt layer (the portable path, constitution
  **P-3.6**).
- **`LayerStore`** (`layer_store.rs`: `create_from_dir`, `resolve_for_artifact`,
  `get_by_digest`) — bakes a resolved dependency tree into a content-addressed layer.
- **Capsule embedding** — `CapsuleMode::Hermetic`/`Replay` already embed layer
  **content** for offline/air-gapped import ("no network needed on import");
  `Thin`/`Headless` carry references only. `CapsuleManifest.included_layers:
  Vec<CapsuleLayerRef>` with `embedded_handle` (`capsule.rs`).
- **`CapsulePlatform`** already records/checks layer **arch+OS compatibility** on
  import — the mechanism that makes native-ELF layers safely portable.

The change (tier-independent):

- Introduce a **lock step** ("bake"): resolve a declared `DependencyPlan` →
  install into a staging dir → `LayerStore::create_from_dir` → pin as a
  `LockedLayerMount` in `runtime.lock`. Same step for every tier; only the **layer
  ABI** differs:
  - **Native tier (bwrap/docker):** native-ELF layers (e.g. `pip install --target`),
    portable to *compatible* host arch/OS (gated by `CapsulePlatform`).
  - **WASM tier:** WASI layers (pure-Python wheels / WASI-built packages),
    **arch-portable** — the only truly "spawn-anywhere" grade. Fail-closed if a
    requested package has no WASI build (no silent fallback to native).
- Define **locked vs dev** dependency modes:
  - **Locked** (default for any agent intended to be exported/stored/spawned
    elsewhere): all deps resolved to `LockedLayerMount`; **no runtime pip**.
    *Required* for `CapsuleMode::Hermetic`/`Replay` export.
  - **Dev/unlocked**: runtime `pip install` via `compose_entrypoint` — convenience
    for iteration only; cannot be Hermetic-exported.
- Net effect: bwrap and docker get embedded-layer deps **immediately** (this is
  worth doing on its own, independent of WASM), and the capsule becomes genuinely
  immutable/portable. WASM is then "one more layer ABI" on the same scheme.

#### 5.4.1 Capsule portability matrix

The outcome is a function of **dependency mode × tier × capsule mode**. Portability
grades: **None** = needs network + resolver at spawn; **Host-compatible** =
self-contained but ELF-bound to a matching arch/OS (`CapsulePlatform`-gated);
**Universal** = arch-independent, spawn-anywhere.

| Dep mode | Tier | `CapsuleMode` | Embeds deps? | Network on import/spawn? | Reproducible? | Portability |
|---|---|---|---|---|---|---|
| **dev** (runtime pip) | native (bwrap/docker) | `Thin`/`Headless` | no (refs only) | **yes** (pip at spawn) | no | **None** |
| **dev** (runtime pip) | native | `Hermetic`/`Replay` | — | — | — | **Rejected** (runtime pip cannot be embedded → export error) |
| **dev** | WASM | any | — | — | — | **N/A** (WASM cannot runtime-pip) |
| **locked** (layers) | native | `Thin`/`Headless` | refs only | yes (fetch layers) | yes (pinned digests) | None–partial (depends on layer source) |
| **locked** (ELF layers) | native | `Hermetic`/`Replay` | **yes** | **no** | **yes** | **Host-compatible** (arch/OS-gated) |
| **locked** (WASI layers) | WASM | `Hermetic`/`Replay` | **yes** | **no** | **yes** | **Universal** (spawn-anywhere) |

Reading the matrix:

- **The only fully immutable, offline, portable rows are the two `locked` +
  `Hermetic`/`Replay` rows** — exactly the "export, store, spawn elsewhere"
  capsule the operator wants.
- **Runtime pip + Hermetic is an explicit error**, not a silent degrade: locked
  mode is a *precondition* the exporter validates before producing a Hermetic
  capsule (enforce in the capsule-export path; see §7).
- **Native locked + Hermetic is already achievable today** with the existing
  `LockedLayerMount` + `CapsuleMode::Hermetic` + `CapsulePlatform` machinery —
  P3 makes it the *default/required* path rather than an opt-in.
- **WASM is the only Universal row** — its sole differentiator over native is
  arch-independence; everything else (embedding, pinning, offline import) is shared.

### 5.5 The WASM backend

- **Runtime:** embed `wasmtime` (Component Model + WASI Preview 2) as a Rust
  dependency. No external binary. (Bundle size / build-time cost noted in §10.)
- **Payload / interpreter:** for `Code{language=Python}`, instantiate a bundled
  `python.wasm` (e.g. VMware Wasm Labs CPython-WASI / `componentize-py`) with the
  agent source as the entry; for advanced use, accept a precompiled `.wasm`
  component directly.
- **WASI context:** `WasiCtxBuilder` with a **preopened** workspace dir (maps
  `agent_dir` → guest `/workspace`, replacing the bind mount), env vars, and piped
  stdin/stdout/stderr (captured into `ExecOutput`).
- **Isolation / constitution mapping:**
  - **P-3.1 / P-3.2** (no network by default; share only with `NetworkAccess`):
    grant **no** socket capabilities by default; networking deferred (see §9).
  - **P-3.7** (CPU/mem/PID/disk quotas): `Store::set_fuel` or epoch interruption
    (CPU), `StoreLimits` (memory/table/instance caps). Cleaner than cgroups.
  - **P-7.21** (SDK bridge rate/size limits): enforced in the `HostFnBridge` wrapper.
  - **P-7.22** (sandbox-escape accounting): WASM has no syscalls to seccomp-deny;
    define WASM-equivalent escape signals — **traps on undeclared imports, denied
    WASI capability calls, resource-limit/fuel exhaustion** — and feed them through
    the existing `record_sandbox_escape_attempt`
    (`gateway_store/observability.rs`) and `detect_sandbox_escape_indicators` path.
  - **P-3.8** (destructive-command denylist in `policy.rs:analyze_command`) runs
    pre-execution and still applies; for `Code` it analyzes source, not a shell line.

### 5.6 Manifest & capability surface (`autonoetic-types` + `autonoetic-gateway`)

Note the crate split: `autonoetic-types` carries only the manifest **string**
(`AgentManifest.runtime.sandbox: String`, `agent.rs:29`); the **enum + parsing**
(`SandboxDriverKind` / `parse`) live in `autonoetic-gateway/src/sandbox.rs`. The
two changes are therefore in different crates:

- **`autonoetic-gateway`** — `SandboxDriverKind::parse` gains `"wasm"` →
  `SandboxDriverKind::Wasm` (`sandbox.rs:152`). (The `agent.rs:29` comment already
  lists `"wasm"` as an intended value — wire it.)
- **`autonoetic-types`** — `AgentManifest.runtime.sandbox = "wasm"` selects the
  portable tier (no struct change; it's already a free-form string).
- Validation (install-time, `runtime/install_contract.rs`): reject a `wasm` agent
  that declares native-only deps or relies on `ExecutionKind::Shell`, with an
  actionable message pointing to the native tier.

## 6. Constitution impact

No signed rule is weakened; all of §3 stays ENFORCED. The work **extends** the
enforcement register to a new driver:

- P-3.1, P-3.2, P-3.6, P-3.7, P-7.21, P-7.22, P-3.8 — re-mapped to WASM mechanisms
  as in §5.5; update the `enforcement_register` (code) and the §3 table’s
  "enforced at" column to cite the WASM backend.
- P-7.22 text already anticipates "driver-equivalents on docker/microvm"; add
  "wasm" equivalents. This is a **doc + register** change describing equivalent or
  tighter isolation — not a constitutional relaxation. Confirm with the operator
  whether it requires a constitution version bump (likely a register-only update).

## 7. Affected code (touch list)

| Area | Files |
|---|---|
| Exec request + backends | `autonoetic-gateway/src/sandbox.rs` (split into `sandbox/{mod,process,wasm,exec_request,sdk_transport}.rs`) |
| SDK transport | `sandbox.rs` (`start_sdk_bridge`, `dispatch_sdk_method`) → transport trait |
| Callers | `runtime/tools/sandbox.rs`, `runtime/tools/artifact_exec.rs`, `runtime/script_execute.rs`, `runtime/middleware.rs` (construct `ExecutionRequest`) |
| Deps/closures | `layer_store.rs`, `runtime_lock.rs`, dependency resolution + bake step (`DependencyPlan` → `LockedLayerMount`) |
| Capsule export | `autonoetic-types/src/capsule.rs` (`CapsuleMode`, `CapsulePlatform`); capsule-export path validates **locked mode** before producing `Hermetic`/`Replay` (reject runtime-pip deps) — see §5.4.1 matrix |
| Manifest | `autonoetic-types/src/agent.rs` (`runtime.sandbox` string), `autonoetic-gateway/src/sandbox.rs` (`SandboxDriverKind::parse`), `runtime/install_contract.rs` (validation) |
| Escape accounting | `runtime/tools/sandbox.rs::detect_sandbox_escape_indicators` (note: in `runtime/tools/`, not top-level `sandbox.rs`), `gateway_store/observability.rs::record_sandbox_escape_attempt`, `scheduler.rs` |
| Constitution | `enforcement_register` + §3 table + `docs/constitution/...` |

## 8. Phasing (each phase independently shippable)

> **Implementation status (2026-06-11).** P4's WASM backend shipped in slices
> (PRs #453/#455/#456/#457): the `sandbox: "wasm"` driver, embedded wasmtime
> (feature-gated), the unified `run_to_output` entry, WASI preopens, stdin, fuel
> + `StoreLimits` resource bounds, a host-capability preflight, and **first-class
> JavaScript agents via Javy** (each JS agent AOT-compiles to a self-contained
> `.wasm`). **`python.wasm` is deferred** (owner decision, 2026-06-11): Python is
> not blocked (it runs on bwrap/docker), the wasm tier would only serve
> pure-stdlib Python (the native-deps ceiling below), and the shared-interpreter
> cost (~20 MB artifact, licensing, provisioning, layering) isn't justified yet.
> The host-function SDK bridge and the WASI layer ABI are likewise deferred —
> they're only needed once a shared interpreter (python.wasm) or sandboxed
> SDK-using wasm agents exist. **Revisit triggers:** a beginner/portability
> scenario requiring agents to run with no bwrap/docker; demand for in-process
> isolation of untrusted pure-Python; a mature WASI-CPython build covering native
> deps. Deferring costs no rework — the tier already runs `_start` modules with
> stdin + a preopened workspace, and the preflight already probes `python3`.

Driver scope per §4.1: **P1–P3 prove the principles on bubblewrap *and* docker
together**; firecracker/microvm is **deferred until after WASM**.

1. **P1 — SDK transport abstraction.** Extract `SdkTransport`; reimplement today's
   socket path behind it and **wire it for docker** (bring docker to bridge parity
   — the current gap at `sandbox.rs:252`). Firecracker bridge deferred.
   *No WASM. Pure refactor + docker parity. Days–1 week.*
2. **P2 — `ExecutionRequest` + `SandboxBackend` trait.** Introduce intent-based
   exec; the `Process` backend renders it for **bwrap and docker**; map
   `ExecutionMode::Script` to `Code{Entry}`. *Multi-backend, cleaner contract. ~1–2 weeks.*
3. **P3 — Pinned, embeddable dependency layers + lock/dev modes (native tier).**
   Add the bake step (`DependencyPlan` → staged install → `LayerStore::create_from_dir`
   → `LockedLayerMount`); make locked mode the default for exportable agents and
   *required* for `CapsuleMode::Hermetic`/`Replay`; keep runtime pip as dev-only.
   **Verified on bwrap and docker** (native-ELF layers) — immutable/portable
   capsules independent of WASM. (The WASI layer ABI is added in P4.) *Weeks.*
4. **P4 — WASM backend.** Embed wasmtime, WASI preopens, stdin, fuel/`StoreLimits`,
   `sandbox: "wasm"` manifest, host-capability preflight, and **JavaScript agents
   via Javy** (per-agent self-contained `.wasm`, no shared interpreter). Tractable
   because the contract it plugs into is already intent-based. **Shipped**
   (#453/#455/#456/#457). *Deferred within P4:* `python.wasm` bundling, the
   host-function SDK bridge, and the WASI layer ABI for P3's bake step — see the
   status note above.
   *Weeks.*
5. **P5 (deferred, after WASM) — firecracker/microvm parity.** Replace the stub
   (`microvm_command` ignores the entrypoint today, `sandbox.rs:1223`): real
   entrypoint execution, SDK transport, dependency layers, and escape-signal
   mapping. Low near-term priority; sequenced last by design.

## 9. Ceilings & open questions

- **Two grades of portability.** Embedded-layer deps make *every* tier's capsule
  immutable and self-contained, but native-ELF layers are portable only to a
  *compatible* host arch/OS (enforced by `CapsulePlatform`). WASI layers are the
  only **arch-portable** ("spawn-anywhere") grade — that's the WASM tier's
  distinguishing value, not dep-embedding itself.
- **Native deps (hard ceiling).** numpy/pandas/most ML have no WASI build →
  those agents stay native (still get embedded native-ELF layers, just not
  arch-portable). The WASM/portable tier is a *subset*, by design.
- **System tools (hard ceiling).** No `git`/`curl`/compilers in WASM. Agents that
  orchestrate CLIs stay native.
- **Agent networking.** `wasi-sockets` (Preview 2) is immature; P4 ships with **no
  agent network** (network-off agents only). Revisit when runtime support matures.
- **Payload curation.** Which `python.wasm` build, what version pinning, bundle
  size (~20 MB) and how it's distributed/locked in `RuntimeLock`.
- **Constitution bump?** Confirm whether the register/§3 update needs a signed
  version bump or is register-only (operator decision; signing is the operator’s).
- **`wasmtime` vs `wasmer`** choice; Component Model maturity for our host-fn SDK.

## 10. Risks

- **Dependency weight:** `wasmtime` adds many transitive crates + compile time +
  binary size. Mitigate behind a Cargo feature (`wasm-tier`) so the native build
  is unaffected.
- **Scope creep into a "WASM does everything" expectation.** Mitigate by framing
  and validating the tier as a documented subset (§9 ceilings) with clear,
  early-failing manifest validation.
- **Two execution paths to maintain.** Mitigate via the shared `SandboxBackend` /
  `SdkTransport` traits so the divergence is contained to backend impls.

## 11. Acceptance criteria

Early-phase criteria are checked on **both bubblewrap and docker** (availability-
gated; see §4.1). Firecracker is out of scope until P5.

- P1: a **docker** agent can call SDK methods (closing the bubblewrap-only gap),
  and **bubblewrap** behavior is byte-for-byte unchanged; transport covered by
  tests gated on `is_bwrap_available()` / `is_docker_available()`.
- P2: `sandbox.exec` and script-mode run through `ExecutionRequest` on **both**
  bwrap and docker; existing integration suites green; no behavior change for
  native agents on either driver.
- P3 (native): on **both bwrap and docker**, a declared-dep agent bakes into a
  pinned `LockedLayerMount`; a `Hermetic` capsule of it imports and runs on a
  compatible host **with no network**; a `Hermetic` export of a runtime-pip
  (dev-mode) agent **fails with an actionable error** (per §5.4.1).
- P4: the WASI layer ABI lands for the bake step (pure-Python deps → pinned WASI
  layers; native-only dep fails closed); a `sandbox: "wasm"` example agent runs a
  pure-Python task **with no `bwrap` and no Docker present**, calls a gateway SDK
  method via host functions, is network-isolated, respects CPU/memory limits, and
  increments escape accounting on a forced trap — verified on Linux and at least
  one non-Linux host.
- P5 (deferred): firecracker/microvm reaches the same P1–P3 bar (SDK transport,
  intent exec, locked layers, Hermetic round-trip).
