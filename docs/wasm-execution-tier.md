# WASM Execution Tier & JavaScript Agents

The **WASM tier** runs an agent's code as a WebAssembly module *in-process* inside
the gateway, instead of spawning an OS sandbox. It is the **portable** execution
tier: an agent that runs here needs no `bwrap`, no `docker`, and no host
interpreter — only the gateway binary. It is also the substrate for **first-class
JavaScript agents**, which compile to a self-contained `.wasm` at install time.

This page covers the concepts, then walks through writing and running a
JavaScript agent end to end.

- New here? Start with [Concepts](#concepts).
- Want to ship a JS agent now? Jump to [Tutorial: a JavaScript agent](#tutorial-3-write-and-run-a-javascript-agent).

Related: [`docs/AGENTS.md`](AGENTS.md) (agent model & script-agent I/O),
[`docs/ARCHITECTURE.md`](ARCHITECTURE.md) (sandbox drivers),
[`docs/rfc/portable-wasm-execution-tier.md`](rfc/portable-wasm-execution-tier.md)
(design & rationale).

---

## Concepts

### Sandbox tiers at a glance

Every agent declares a sandbox in its manifest (`metadata.autonoetic.runtime.sandbox`).
The gateway dispatches each tier differently:

| Tier (`sandbox:`) | Isolation | Host requirement | Network | Best for |
|---|---|---|---|---|
| `bubblewrap` (default) | OS user namespaces | `bwrap` on PATH | optional (off by default) | general Linux agents, system tools |
| `docker` | container | `docker` on PATH | optional | reproducible images, non-namespace hosts |
| `microvm` | firecracker VM | `firecracker` (stubbed) | — | (future) strong VM isolation |
| `wasm` | in-process WASI sandbox | **none** (built-in) | **none** | portable / beginner agents, JavaScript agents |

The WASM tier's distinguishing value is **arch-portable, zero-host-dependency
execution**: the module is run by the embedded `wasmtime` engine, so the same
agent runs anywhere the gateway runs, with capability-based isolation and no
external sandbox tool to install.

### How the WASM tier runs an agent

A `sandbox: "wasm"` agent's `script_entry` is a WebAssembly module. The gateway
runs it through the unified execution entry (`SandboxRunner::run_to_output`),
which for the wasm tier:

1. Reads the declared `.wasm` entry from the agent bundle (rejecting absolute or
   `..` paths).
2. Instantiates it with `wasmtime` as a **WASI Preview 1 command** (calls the
   module's `_start`).
3. **Preopens** the agent's workspace directory at the same guest path the
   process tiers use, so input-file environment variables resolve identically
   across tiers.
4. Passes the agent's `args`, environment, and **stdin** through; captures
   **stdout** and **stderr** as the result.
5. Grants **no network**.

The script-agent I/O contract is identical to the other tiers (see
[`docs/AGENTS.md`](AGENTS.md)): input arrives on stdin (default) or as argv
(`script_input_mode: args`), and stdout is the agent's result.

### Resource bounds (constitution P-3.7)

Each wasm run is bounded so a runaway module can't spin or balloon:

- **Fuel** caps CPU work (instruction count). Exhaustion is a clear error, not a
  hang.
- **Memory** is capped via `StoreLimits`.

Defaults (`WasmLimits`: 20,000,000,000 fuel units (20B), 512 MiB) are generous enough for real
interpreter runs and live in code, not the signed constitution.

### Determinism & content-addressing

Compiled wasm entries are stored as ordinary bundle files, so they are
**content-addressed** in the agent revision (the revision digest covers them).
For JavaScript agents, the gateway compiles with Javy's `-C deterministic=y`
(fixed clocks, zero-filled RNG during pre-initialization), so identical source
yields an identical module — rebuilds don't churn the revision digest.

### JavaScript agents (via Javy)

JavaScript is supported by compiling each agent's JS to a **self-contained**
`.wasm` module with [Javy](https://github.com/bytecodealliance/javy) at **bootstrap**
(install) time. Javy embeds the QuickJS engine *into the module*, so:

- There is **no shared interpreter** to ship or manage — each agent's `.wasm` is
  standalone and runs on the `_start` path above.
- The compiled module is content-addressed in the revision like any other file.

When you bootstrap an agent whose `script_entry` ends in `.js`/`.mjs`, the
gateway:

1. Requires `sandbox: "wasm"` (JS agents run on the wasm tier).
2. Requires `javy` on PATH (else bootstrap fails with an install hint).
3. Runs `javy build <entry> -o <stem>.wasm -C deterministic=y`.
4. Bundles the `.wasm` and repoints the manifest's `script_entry` at it.

The runtime then executes the `.wasm` unchanged — it needs no JavaScript awareness.

### What runs, and what doesn't (ceilings)

The WASM tier is a deliberate **subset** of what the native tiers can do:

- **JavaScript:** QuickJS is ES2020-ish. **No Node.js APIs** (`fs`, `net`,
  `http`, …) and **no npm packages with native bindings**. I/O is stdin/argv →
  stdout. No JIT (interpreted), so it's slower than V8 — fine for orchestration
  and glue, not number crunching.
- **No network** (WASI sockets are not granted).
- **No system tools** (`git`, `curl`, compilers) — agents that orchestrate CLIs
  stay on a native tier.
- **Python on the wasm tier is not supported** (deferred — see below). Python
  agents run on `bubblewrap`/`docker`.

If your agent needs any of the above, use `bubblewrap` or `docker`.

### The `wasm-tier` build feature

The wasm engine is gated behind a Cargo feature so the default build never pays
wasmtime's compile-time and binary-size cost:

```bash
# default build: no wasm tier (wasmtime not compiled in)
cargo build -p autonoetic

# with the wasm tier (required to run sandbox: "wasm" agents)
cargo build -p autonoetic --features wasm-tier
```

A gateway built **without** the feature will reject a `sandbox: "wasm"` agent
with a clear "requires the `wasm-tier` build feature" error, and the preflight
(below) shows the wasm tier as unavailable.

### Host capability preflight

Because tiers and language toolchains depend on host tools, the gateway probes
them at startup and on demand:

```bash
autonoetic gateway preflight          # human-readable
autonoetic gateway preflight --json   # machine-readable
```

```
Host capabilities — sandbox tiers:
  [ok] sandbox: bubblewrap (bwrap on PATH)
  [ok] sandbox: docker (docker on PATH)
  [--] sandbox: microvm (firecracker on PATH)
  [ok] sandbox: wasm (wasm-tier build feature)
Host capabilities — language toolchains:
  [ok] language: python (python3 on PATH)
  [ok] language: javascript (wasm via javy) (javy on PATH)
  [--] language: javascript (process via node) (node on PATH)
```

The same summary is logged at `gateway start`; `preflight` exits non-zero when no
sandbox tier is runnable at all.

---

## Tutorials

### Tutorial 1: check your host

```bash
# Build with the wasm tier and probe the host.
cargo run -p autonoetic --features wasm-tier -- gateway preflight
```

For a JavaScript agent you want both `[ok] sandbox: wasm` (you built with the
feature) and `[ok] language: javascript (wasm via javy)` (Javy is installed).

### Tutorial 2: install Javy

Javy is the JS→wasm compiler. Install the release binary (x86_64 Linux shown):

```bash
gh release download --repo bytecodealliance/javy \
  --pattern 'javy-x86_64-linux-v*.gz' --dir /tmp --clobber
gunzip -f /tmp/javy-x86_64-linux-v*.gz
install -m 755 /tmp/javy-x86_64-linux-v* ~/.local/bin/javy
javy --version
```

Ensure the install dir is on the same `PATH` the gateway sees (`~/.local/bin`,
or use `/usr/local/bin` with `sudo`). Confirm with `autonoetic gateway preflight`.

### Tutorial 3: write and run a JavaScript agent

A JavaScript agent is a script agent whose `script_entry` is a `.js`/`.mjs`
file and whose `sandbox` is `wasm`. It needs three files in its directory.

**1. `main.js`** — read the task input from stdin, write the result to stdout:

```javascript
// Read all of stdin synchronously via the Javy host's IO API, then echo a
// greeting on stdout. Plain ES2020 — no Node `require`, no npm.
function readStdin() {
  const STDIN = 0;
  const buf = new Uint8Array(1024);
  const chunks = [];
  let n;
  while ((n = Javy.IO.readSync(STDIN, buf)) > 0) {
    chunks.push(buf.slice(0, n));
  }
  const total = chunks.reduce((a, c) => a + c.length, 0);
  const all = new Uint8Array(total);
  let o = 0;
  for (const c of chunks) { all.set(c, o); o += c.length; }
  return new TextDecoder().decode(all);
}

const task = readStdin().trim();
console.log(JSON.stringify({ greeting: `hello, ${task || "world"}` }));
```

> The Javy host exposes `Javy.IO.readSync(fd, Uint8Array)` / `writeSync` for raw
> stdio; `console.log` writes to stdout and is the simplest way to emit your
> result. Keep to plain ES2020 — no Node APIs, no npm.

**2. `SKILL.md`** — declare a script agent on the wasm tier:

```yaml
---
name: "hello-js.default"
description: "A tiny JavaScript agent that greets its input."
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "wasm"          # JavaScript agents run on the wasm tier
      runtime_lock: "runtime.lock"
    agent:
      id: "hello-js.default"
      name: "Hello JS"
      description: "Greets its input."
    execution_mode: "script"
    script_entry: "main.js"     # compiled to main.wasm at bootstrap
    script_input_mode: "stdin"  # task payload arrives on stdin
    capabilities: []
---
# Hello JS
A minimal JavaScript script agent. Reads a name from stdin, returns a greeting.
```

**3. `runtime.lock`** — the minimal pinned closure:

```yaml
gateway:
  artifact: "marketplace://gateway/autonoetic-gateway"
  version: "0.1.0"
  sha256: "replace-me"
sdk:
  version: "0.1.0"
sandbox:
  backend: "wasm"
dependencies: []
artifacts: []
layers: []
```

**4. Bootstrap it.** Place the directory under your configured `agents_dir` and
bootstrap — this is where the JS is compiled to wasm:

```bash
cargo run -p autonoetic --features wasm-tier -- agent bootstrap
```

At this step the gateway runs `javy build main.js -o main.wasm -C deterministic=y`,
bundles `main.wasm`, and repoints `script_entry` to it in the stored revision. If
`javy` is missing or `sandbox` isn't `wasm`, bootstrap fails with an explanatory
error.

**5. Run it.** Start the gateway (with the feature) and invoke the agent:

```bash
cargo run -p autonoetic --features wasm-tier -- gateway start &      # in one shell
cargo run -p autonoetic --features wasm-tier -- agent run hello-js.default "Ada"
```

The agent's QuickJS module runs in-process; you get back
`{"greeting":"hello, Ada"}`.

### Tutorial 4: input and output

JavaScript agents follow the same script-agent contract as Python/shell agents:

- **`script_input_mode: stdin`** (default): the normalized task payload is written
  to the module's stdin (read it as shown above).
- **`script_input_mode: args`**: the payload is passed as the first CLI argument
  (`scriptArgs[0]` in Javy).
- The environment variables `AUTONOETIC_INPUT_PATH` / `AUTONOETIC_INPUT` and the
  preopened workspace are available, exactly as on the native tiers.
- **stdout** is the agent's result; if the manifest declares `io.returns`, the
  output must match that schema.

### Tutorial 5: troubleshooting

| Symptom | Cause & fix |
|---|---|
| Bootstrap: *"needs the Javy compiler (`javy`) on PATH"* | Install Javy (Tutorial 2); confirm with `gateway preflight`. |
| Bootstrap: *"declares a JavaScript entry … but sandbox '…'"* | Set `sandbox: "wasm"` in the manifest. |
| Run: *"requires the `wasm-tier` build feature"* | Rebuild the gateway with `--features wasm-tier`. |
| Run: *"wasm execution failed (trap or resource limit)"* | The module hit the fuel/memory bound, or threw. Check the JS for infinite loops / runaway allocation. |
| `ReferenceError` for a Node/browser API | QuickJS has no Node APIs or DOM. Use plain ES2020 + `Javy.IO`/`console`. |
| Agent hangs reading input | In `stdin` mode, read until EOF (`readSync` returns 0). Don't block on more input than is sent. |

---

## Status & roadmap

Shipped (PRs #453/#455/#456/#457): the `sandbox: "wasm"` driver, embedded
wasmtime (feature-gated), unified `run_to_output`, WASI preopens, stdin, resource
bounds, the host-capability preflight, and JavaScript agents via Javy.

**Deferred:** `python.wasm` (a shared CPython-in-wasm interpreter). Python is not
blocked — it runs on `bubblewrap`/`docker` — and a wasm CPython would only serve
pure-stdlib Python (no native deps) at a high cost (~20 MB shared artifact,
licensing, provisioning). Revisit if a concrete no-bwrap/docker portability need,
untrusted-pure-Python isolation need, or a mature native-deps WASI-CPython build
appears. See the status note in
[`docs/rfc/portable-wasm-execution-tier.md`](rfc/portable-wasm-execution-tier.md).
