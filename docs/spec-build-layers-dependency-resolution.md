# Spec: Build Layers & Dependency Resolution

**Status:** Draft
**Author:** Architecture Review
**Date:** 2026-03-27

---

## 1. Problem Statement

### 1.1 The Demo Loop

When a planner delegates "fetch weather in Paris" to specialist agents:

```
coder → writes weather_fetcher.py (imports httpx) + requirements.txt
       → artifact.build(["weather_fetcher.py", "requirements.txt"])
         (packages files only, no dependency installation)

evaluator → sandbox.exec("python3 weather_fetcher.py Paris")
          → ModuleNotFoundError: No module named 'httpx'
          → tries pip install manually → no network in bubblewrap
          → tries --break-system-packages → permission denied
          → tries venv → no network to reach PyPI
          → loops for 39+ turns, 9 errors, eventually times out
```

### 1.2 Root Cause

Three disconnected phases with no bridge:

| Phase | Has Files | Has Network | Can Install Deps |
|-------|-----------|-------------|-----------------|
| `artifact.build` | Yes (packages them) | No | No |
| `sandbox.exec` (bubblewrap) | Yes (mounts artifact) | No (`--unshare-all`) | No |
| `sandbox.exec` (with `dependencies` param) | Yes | No (venv + pip but no egress) | No |

The `compose_entrypoint` function prepends `pip install` but the sandbox has no network, so pip can never reach PyPI.

### 1.3 Scale Mismatch

Even if network worked, the current model breaks at scale:

- `httpx` ≈ 50 files → barely feasible as individual content blobs
- `numpy` ≈ 800 files with `.so` binaries → impractical
- `node_modules` ≈ 30,000 files → impossible

Content store is designed for agent-authored source files (small, few). Dependencies are directory trees (large, many). The artifact manifest is a flat list of `ArtifactFileEntry` — one entry per file.

### 1.4 Remote Agent Concern

Remote agents communicate over HTTP. Shipping dependency trees as individual content blobs over JSON-RPC is a non-starter for large packages.

---

## 2. Design

### 2.1 Core Concept: Layers

A **layer** is an opaque, compressed directory tree stored by the gateway. Layers are content-addressed (SHA-256 over the compressed archive) and deduplicated automatically.

Artifacts gain an optional `layers` field referencing zero or more layers. When the sandbox mounts an artifact, it extracts referenced layers at declared mount paths alongside the flat file mounts.

**Gateway responsibility**: store, verify, mount. Nothing more.
**Agent responsibility**: decide what goes into a layer, how to build it, where to mount it.

### 2.2 Storage Layout

```
.gateway/
├── content/sha256/...          # existing content blobs (source files)
├── artifacts/                  # existing artifact manifests
│   └── art_1756ca5a/
│       └── manifest.json       # gains optional "layers" field
└── layers/                     # NEW
    └── layer_<hash>/
        └── contents.tar.zst    # compressed directory tree
```

### 2.3 Artifact Manifest Extension

```jsonc
{
  "artifact_id": "art_1756ca5a",
  "files": [
    {"name": "weather_fetcher.py", "handle": "sha256:...", "alias": "abc12345"}
  ],
  "layers": [
    {
      "layer_id": "layer_a1b2c3d4",
      "name": "python-deps",
      "mount_path": "/tmp/deps",
      "digest": "sha256:..."
    }
  ],
  "entrypoints": ["weather_fetcher.py"],
  "digest": "sha256:...",
  "created_at": "...",
  "builder_session_id": "...",
  "reused": false
}
```

`layers` is optional. Artifacts without layers behave exactly as today.

### 2.4 New Tool: `layer.create`

Agents with `WriteAccess` can create layers. The tool captures a directory from inside a sandbox and stores it as a layer.

```jsonc
// Agent calls sandbox.exec with network + capture_paths
sandbox.exec({
  "command": "pip install -r requirements.txt -t /tmp/deps",
  "capture_paths": ["/tmp/deps"]
})
// → returns { ok: true, captured_layers: [{ "layer_id": "layer_a1b2c3d4", ... }] }
```

Wait — this conflates two things. Let me reframe.

The builder agent runs in a **network-enabled sandbox** (declared in its SKILL.md). It:

1. Calls `sandbox.exec` to install deps (has network because builder's sandbox allows it)
2. Files written inside the sandbox are ephemeral — lost when the sandbox exits
3. Needs a way to get those files out

**Option chosen**: `sandbox.exec` gains `capture_paths`. After execution, the gateway captures the listed paths as a layer.

```jsonc
sandbox.exec({
  "command": "pip install -r requirements.txt -t /tmp/deps",
  "capture_paths": [
    {"path": "/tmp/deps", "mount_as": "/tmp/deps"}
  ]
})
```

Returns:
```jsonc
{
  "ok": true,
  "exit_code": 0,
  "stdout": "...",
  "stderr": "...",
  "captured_layers": [
    {
      "layer_id": "layer_a1b2c3d4",
      "name": "deps",
      "mount_path": "/tmp/deps",
      "digest": "sha256:...",
      "file_count": 52,
      "size_bytes": 1234567
    }
  ]
}
```

**Gateway behavior for `capture_paths`**:
1. Run the sandbox command normally
2. After completion, for each `capture_path`, tar+compress the directory inside the sandbox workspace
3. Compute SHA-256 digest
4. Store as `.gateway/layers/layer_<hash>/contents.tar.zst`
5. Dedup: if layer with same digest exists, reuse it
6. Return layer metadata in the response

This is a dumb operation: tar, compress, hash, store. No language detection, no parsing.

### 2.5 `artifact.build` Extension

```jsonc
artifact.build({
  "inputs": ["weather_fetcher.py"],
  "entrypoints": ["weather_fetcher.py"],
  "layers": [
    {"layer_id": "layer_a1b2c3d4", "name": "python-deps", "mount_path": "/tmp/deps"}
  ]
})
```

The gateway:
1. Validates that each referenced layer exists and digest matches
2. Records layers in the artifact manifest
3. The artifact's deterministic ID now incorporates layer digests (so different deps → different artifact)

### 2.6 `sandbox.exec` with Layer-Aware Artifacts

When `sandbox.exec` runs with an `artifact_id` that has layers:

1. Mount flat files as today (read-only bind from content store)
2. For each layer, extract `contents.tar.zst` into a temp dir and bind-mount at `mount_path`
3. Execute the command

The evaluator just runs:
```jsonc
sandbox.exec({
  "artifact_id": "art_1756ca5a",
  "command": "PYTHONPATH=/tmp/deps python3 weather_fetcher.py Paris"
})
```

No network needed. No pip install. Everything is self-contained.

### 2.7 Build Agent

A new agent role (or extension of an existing one) responsible for build-time dependency resolution.

**Option A**: New `builder.default` agent
**Option B**: Extend `specialized_builder.default` with build responsibilities

Recommendation: **Option A** — a focused build agent. The specialized_builder already has a complex job (installing durable agents). Dependency resolution for artifacts is a different concern.

Builder agent profile:

```yaml
capabilities:
  - type: "SandboxFunctions"
    allowed: ["content.", "artifact.", "sandbox.exec"]
  - type: "ReadAccess"
    scopes: ["self.*", "session/*"]
  - type: "WriteAccess"
    scopes: ["self.*", "session/*"]
  - type: "CodeExecution"        # to run pip/npm/etc.
```

The builder's sandbox must have network access. This is controlled by the gateway's sandbox configuration for the builder's agent directory, not by a runtime flag from the agent.

**Network access for the builder**:
- The builder agent directory gets a `sandbox.conf` (or env var) that tells the gateway to use `--share-net` for this agent's sandbox executions
- This is a deployment/admin decision, not an agent capability
- The builder does NOT get `NetworkAccess` capability in the general sense — it only gets network during `sandbox.exec` for dep installation
- Runtime code execution (by evaluator) still goes through the normal approval flow if it needs network

Alternative: extend the existing `SandboxConfig` with a per-agent `share_net` flag:

```toml
# agents/builder.default/sandbox.conf
share_net = true
```

The gateway reads this when spawning sandboxes for this agent. Other agents remain isolated.

### 2.8 Workflow

Complete flow for the weather demo:

```
1. planner receives "what is the weather in Paris?"
2. planner delegates to architect → design
3. planner delegates to coder → weather_fetcher.py + requirements.txt
4. planner delegates to builder:
     "Install deps from requirements.txt and build artifact with deps"

   builder:
     a. content.read("requirements.txt")          # get dep list
     b. sandbox.exec({
          "command": "pip install -r requirements.txt -t /tmp/deps",
          "capture_paths": [{"path": "/tmp/deps", "mount_as": "/tmp/deps"}]
        })
        → captured_layers: [{layer_id: "layer_abc123", ...}]
     c. artifact.build({
          "inputs": ["weather_fetcher.py"],
          "layers": [{"layer_id": "layer_abc123", "name": "python-deps", "mount_path": "/tmp/deps"}],
          "entrypoints": ["weather_fetcher.py"]
        })
        → artifact art_with_deps

5. planner delegates to evaluator:
     "Validate artifact art_with_deps, run weather_fetcher.py Paris"

   evaluator:
     a. sandbox.exec({
          "artifact_id": "art_with_deps",
          "command": "PYTHONPATH=/tmp/deps python3 weather_fetcher.py Paris"
        })
        → runs successfully, returns weather data
```

---

## 3. Implementation Details

### 3.1 Layer Storage

```rust
pub struct LayerStore {
    layers_dir: PathBuf,
}

pub struct LayerManifest {
    pub layer_id: String,
    pub digest: String,       // SHA-256 of the tar.zst
    pub file_count: usize,
    pub size_bytes: u64,
    pub created_at: String,
}

impl LayerStore {
    pub fn create_from_dir(&self, source_dir: &Path) -> anyhow::Result<LayerManifest>;
    pub fn extract_to(&self, layer_id: &str, target_dir: &Path) -> anyhow::Result<()>;
    pub fn inspect(&self, layer_id: &str) -> anyhow::Result<LayerManifest>;
    pub fn exists(&self, digest: &str) -> bool;
}
```

### 3.2 Capture Mechanism

`sandbox.exec` with `capture_paths` needs the sandbox workspace to persist after command execution so the gateway can read the captured paths. Current flow:

```
spawn sandbox → run command → wait_with_output → drop sandbox
```

New flow when `capture_paths` is provided:

```
spawn sandbox → run command → wait_with_output
  → for each capture_path:
      → tar+compress the path from sandbox workspace
      → store as layer
  → drop sandbox
```

This requires the sandbox workspace directory to survive until capture is complete. For bubblewrap, the workspace is the agent dir bind-mounted at `/tmp`. Files written inside `/tmp` in the sandbox are written to the host's agent dir temp space. The gateway reads them before cleanup.

### 3.3 Deterministic Artifact ID with Layers

Current: `SHA-256(sorted file handles + sorted entrypoints)`
New: `SHA-256(sorted file handles + sorted entrypoints + sorted layer digests)`

Same source files + different dependency versions → different artifact ID. Correct behavior.

### 3.4 Sandbox Mount Changes

`sandbox.exec` with `artifact_id` currently:
```rust
fn resolve_files(artifact_id) → Vec<(name, content_bytes)>
// Write each to temp, bind-mount at /tmp/<name>
```

New behavior:
```rust
fn resolve_files(artifact_id) → Vec<(name, content_bytes)>
fn resolve_layers(artifact_id) → Vec<(mount_path, layer_id)>

// For files: same as today
// For layers: extract tar.zst to temp dir, bind-mount at mount_path
```

### 3.5 Size Limits

Configurable per-gateway:
- `max_layer_size_bytes` — default 500 MB
- `max_layers_per_artifact` — default 5
- `max_total_layer_size_bytes` — default 2 GB

Layer creation fails if limits exceeded. This prevents a malicious/buggy agent from filling disk.

---

## 4. Remote Agent Compatibility

### 4.1 Layer Transfer

Remote agents can't write directly to the gateway filesystem. Options:

**Phase 1 (current)**: Remote agents are not builders. Layers are created by local builder agents only.

**Phase 2 (future)**: Add HTTP endpoints:
- `POST /api/v1/layers` — upload a layer archive
- `GET /api/v1/layers/{layer_id}` — download a layer archive

The builder agent runs locally. Remote evaluators just reference artifact IDs with layers — the gateway handles mounting.

### 4.2 Cross-Gateway

When an artifact needs to run on a different gateway node (federation via OFP), the layer archive travels with the artifact. The cognitive capsule export already bundles artifact files; layers would be added to the export format.

---

## 5. Security Considerations

### 5.1 Supply Chain

Layers contain installed packages — a supply chain attack vector. Mitigations:

- Layer digest is content-addressed — tampering is detected
- Auditor agent inspects the artifact manifest (including layer references)
- `promotion.record` findings can flag suspicious packages
- Future: pin dependency hashes in `runtime.lock`, verify at layer creation

### 5.2 Network Isolation

The builder agent's sandbox has network. Principles:

- Network is granted at the **sandbox configuration** level, not as an agent capability
- The builder can only reach the network during `sandbox.exec` — all other tool calls go through the gateway's normal policy engine
- The builder cannot exfiltrate data — content.write/read are still gateway-mediated
- Other agents (evaluator, coder, etc.) remain fully isolated

### 5.3 Layer Tampering

Layers are immutable (read-only after creation). The digest is verified:
- At artifact.build time (layers referenced must exist with matching digest)
- At sandbox mount time (layer archive is verified before extraction)

---

## 6. Alternatives Considered

### 6.1 `build_command` on `artifact.build`

Let agents pass a build command to `artifact.build` that runs in a network-enabled sandbox before packaging.

**Rejected because**: Makes `artifact.build` into a build system. The gateway gains implicit knowledge of "build phases." Violates the neutral executor principle.

### 6.2 Sandbox Reuse

Persist a sandbox across multiple `sandbox.exec` calls. Install deps once, reuse the sandbox.

**Rejected because**: Bubblewrap has no "save image" concept. Docker could do this but couples design to one driver. Artifacts (content-addressed, portable) are the natural reusable unit, not sandboxes.

### 6.3 Pre-baked Docker Images

Use Docker images with common deps pre-installed.

**Rejected because**: Couples to Docker driver. Not content-addressed. No dedup. Doesn't generalize to bubblewrap or microvm.

### 6.4 Agent Writes Every File via content.write

Builder does `content.write` for every file in `node_modules/`.

**Rejected because**: 30,000 content.write calls. Infeasible for LLM-driven agents.

---

## 7. Open Questions

1. **Layer naming**: Should `layer_id` be derived from content digest alone, or also include the mount path? Current proposal: digest-only (same deps mounted at different paths = same layer).

2. **Layer garbage collection**: When an artifact is deleted, should its layers be cleaned up? Reference counting? Or layers are permanent (dedup makes this cheap)?

3. **Builder sandbox config**: Should `share_net` be an agent-level config file, a gateway-level config, or a per-session flag? Current proposal: agent-level `sandbox.conf` in agent directory, set by admin during agent deployment.

4. **PYTHONPATH / NODE_PATH**: Who is responsible for setting these — the agent in the command string, or the gateway via a manifest field? Current proposal: agent's responsibility (keeps gateway dumb).

5. **Layer compression format**: `tar.zst` proposed for speed + ratio. Acceptable dependency for the Rust workspace?

---

## 8. Success Criteria

- [ ] The weather demo runs end-to-end without evaluator looping
- [ ] Artifacts with no layers behave identically to current behavior
- [ ] Layer dedup works: same deps → same layer ID
- [ ] Evaluator never needs network for dependency installation
- [ ] Builder agent completes dep installation in < 3 turns
- [ ] Layer extraction at sandbox mount completes in < 2s for typical Python deps
- [ ] No gateway intelligence about languages, package managers, or build systems
