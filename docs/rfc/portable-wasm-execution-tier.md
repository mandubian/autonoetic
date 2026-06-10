# RFC: Portable WASM Execution Tier

- **Status:** Draft (for review)
- **Author:** (proposed by operator; drafted with Claude Code)
- **Scope:** `autonoetic-gateway` sandbox/execution layer, `autonoetic-types` manifest + runtime lock, agent SDK
- **Related:** `docs/cognitive-capsule.md`, `docs/content-store.md`, `docs/ARCHITECTURE.md`, constitution §3 (`docs/constitution/versions/<latest>/constitution.md`)

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
Every driver returns a host `(program, argv)` that runs `sh -c "<entrypoint>"`:

```
spawn_with_driver_and_dependencies_and_env(driver, agent_dir, entrypoint,
    dependencies: Option<&DependencyPlan>, overrides, extra_env, root_session_id)
  → match driver {
        Bubblewrap => bubblewrap_shell_command(...),   // sandbox.rs:265
        Docker     => docker_command(...),             // sandbox.rs:277
        MicroVm    => microvm_command(...),            // sandbox.rs:278
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
    /// Bubblewrap/docker: bind-mounted Unix socket (today's path).
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

### 5.4 Evolution B′ — Dependencies as ahead-of-time WASI closures

WASM cannot `pip install`. Extend dependency resolution to a **prebuilt-layer**
model that reuses the existing layer/closure machinery:

- `LayerStore` (`autonoetic-gateway/src/layer_store.rs`: `create_from_dir`,
  `resolve_for_artifact`, `get_by_digest`) already provides content-addressed,
  read-only layers (constitution **P-3.6**).
- `RuntimeLock.layers: Vec<LockedLayerMount>` (`runtime_lock.rs`) already pins the
  execution closure.
- Add a resolution step: for a WASM agent, declared `DependencyPlan.packages` are
  resolved **at build time** to prebuilt **WASI-compatible** layer digests (pure-
  Python wheels or WASI-built packages), pinned in `RuntimeLock.layers`, and
  mounted read-only via WASI preopens. **Fail-closed** with a clear error if a
  requested package has no WASI build (no silent fallback to native).
- The native tier keeps runtime pip; this AOT path also improves native-tier
  reproducibility and is therefore worth doing on its own.

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

### 5.6 Manifest & capability surface (`autonoetic-types`)

- `SandboxDriverKind::parse` gains `"wasm"` → `SandboxDriverKind::Wasm`
  (`sandbox.rs:152`). (The `agent.rs:29` comment already lists `"wasm"` — wire it.)
- `AgentManifest.runtime.sandbox = "wasm"` selects the portable tier.
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
| Deps/closures | `layer_store.rs`, `runtime_lock.rs`, dependency resolution |
| Manifest | `autonoetic-types/src/agent.rs` (`SandboxDriverKind`, validation), `runtime/install_contract.rs` |
| Escape accounting | `gateway_store/observability.rs`, `scheduler.rs`, `sandbox.rs::detect_sandbox_escape_indicators` |
| Constitution | `enforcement_register` + §3 table + `docs/constitution/...` |

## 8. Phasing (each phase independently shippable)

1. **P1 — SDK transport abstraction.** Extract `SdkTransport`; reimplement today's
   socket path behind it; wire it for docker/microvm too (closes existing gap).
   *No WASM. Pure refactor + bug-fix. Days–1 week.*
2. **P2 — `ExecutionRequest` + `SandboxBackend` trait.** Introduce intent-based
   exec; `Process` backend renders it; map `ExecutionMode::Script` to `Code{Entry}`.
   *Multi-backend ready, cleaner contract. ~1–2 weeks.*
3. **P3 — AOT WASI dependency closures.** Resolve declared deps to prebuilt WASI
   layers pinned in `RuntimeLock`; fail-closed on missing WASI build. *Weeks.*
4. **P4 — WASM backend.** Embed wasmtime, bundle `python.wasm`, host-function SDK,
   WASI preopens, fuel/`StoreLimits`, escape signals, `sandbox: "wasm"` manifest.
   Tractable because the contract it plugs into is already intent-based. *Weeks.*

## 9. Ceilings & open questions

- **Native deps (hard ceiling).** numpy/pandas/most ML have no WASI build →
  those agents stay native. The portable tier is a *subset*, by design.
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

- P1: docker/microvm agents can call SDK methods; existing bubblewrap behavior
  byte-for-byte unchanged; transport covered by tests.
- P2: `sandbox.exec` and script-mode run through `ExecutionRequest`; bubblewrap
  integration suite green; no behavior change for native agents.
- P3: a WASM-targeted agent resolves pure-Python deps to pinned WASI layers; a
  native-only dep fails closed with an actionable error.
- P4: a `sandbox: "wasm"` example agent runs a pure-Python task **with no `bwrap`
  and no Docker present**, calls a gateway SDK method via host functions, is
  network-isolated, respects CPU/memory limits, and increments escape accounting
  on a forced trap — verified on Linux and at least one non-Linux host.
