# Implementation Plan: Build Layers & Dependency Resolution

**Spec:** `docs/spec-build-layers-dependency-resolution.md`
**Date:** 2026-03-27

---

## Phase 1: Layer Store (Foundation)

No agent-facing changes. Pure storage primitive.

### Task 1.1: `LayerStore` module

**File:** `autonoetic-gateway/src/layer_store.rs`

- [x] Create `LayerStore` struct with `layers_dir: PathBuf`
- [x] `create_from_dir(source_dir: &Path) -> LayerManifest` — tar + zstd compress + SHA-256 + store
- [x] `extract_to(layer_id: &str, target_dir: &Path)` — verify digest + decompress
- [x] `inspect(layer_id: &str) -> LayerManifest` — load manifest
- [x] `exists(layer_id: &str) -> bool` — check existence
- [x] Dedup: if content digest matches existing layer, return existing layer_id
- [x] Size limit checks against configurable max
- [x] Add `pub mod layer_store;` to `lib.rs`

**Tests:**
- [ ] Create layer from directory, verify manifest fields
- [ ] Extract layer, verify file contents match source
- [ ] Dedup: same dir → same layer_id
- [ ] Different dir → different layer_id
- [ ] Missing layer returns error
- [ ] Size limit exceeded returns error
- [ ] Digest verification on extract (tampered archive fails)

**Dependencies:** Add `tar`, `zstd` (or `flate2` as fallback) to `Cargo.toml`

### Task 1.2: `LayerManifest` type

**File:** `autonoetic-types/src/layer.rs`

- [x] Define `LayerManifest` struct: `layer_id`, `digest`, `file_count`, `size_bytes`, `created_at`
- [x] Define `ArtifactLayer` struct: `layer_id`, `name`, `mount_path`, `digest`
- [x] Add `pub mod layer;` to types `lib.rs`

**Tests:**
- [ ] Serialization round-trip

### Task 1.3: Update `ArtifactBundle` type

**File:** `autonoetic-types/src/artifact.rs`

- [x] Add `layers: Vec<ArtifactLayer>` to `ArtifactBundle` (default empty vec, backward compatible)
- [x] Update `compute_deterministic_artifact_id` to include layer digests in hash input

**Tests:**
- [ ] Artifact with layers has different ID than artifact without
- [ ] Artifact with same files + same layers = same ID
- [ ] Deserializing old manifests (no `layers` field) produces empty vec

---

## Phase 2: `capture_paths` on `sandbox.exec`

### Task 2.1: Extend `SandboxExecArgs`

**File:** `autonoetic-gateway/src/runtime/tools.rs`

- [x] Add `capture_paths: Option<Vec<CapturePath>>` to `SandboxExecArgs`
- [x] Define `CapturePath` struct: `path: String`, `mount_as: String`

### Task 2.2: Implement capture logic

**File:** `autonoetic-gateway/src/runtime/tools.rs` (sandbox.exec handler)

After `runner.process.wait_with_output()`:
- [x] If `capture_paths` is Some, iterate paths
- [x] For each path, look for it in the sandbox workspace (bubblewrap: agent dir temp space)
- [x] Call `layer_store.create_from_dir()` for each
- [x] Append `captured_layers` to the JSON response
- [x] Clean up temp files

**Tests:**
- [ ] `sandbox.exec` with `capture_paths` returns `captured_layers`
- [ ] Captured layer contains expected files
- [ ] `sandbox.exec` without `capture_paths` has no `captured_layers` in response
- [ ] Capture path that doesn't exist returns error
- [ ] Multiple capture paths produce multiple layers

### Task 2.3: Tool definition update

**File:** `autonoetic-gateway/src/runtime/tools.rs`

- [x] Add `capture_paths` to `sandbox.exec` input schema in tool definition
- [x] Update description to mention layer capture capability

---

## Phase 3: Layer-Aware `artifact.build`

### Task 3.1: Accept `layers` parameter

**File:** `autonoetic-gateway/src/artifact_store.rs`

- [ ] Extend `build()` signature to accept optional `layers: Vec<ArtifactLayer>`
- [ ] Validate each layer exists in `LayerStore` and digest matches
- [ ] Include layers in the persisted manifest
- [ ] Include layer digests in deterministic artifact ID computation

**Tests:**
- [ ] Build with layers succeeds when layers exist
- [ ] Build fails if referenced layer doesn't exist
- [ ] Build fails if layer digest doesn't match
- [ ] Artifact with layers has different ID than without
- [ ] Dedup: same files + same layers = same artifact (reused: true)

### Task 3.2: Extend tool definition

**File:** `autonoetic-gateway/src/runtime/tools.rs`

- [ ] Add `layers` field to `artifact.build` input schema
- [ ] Parse `layers` arg in handler, pass to `artifact_store.build()`

---

## Phase 4: Layer-Aware Sandbox Mounting

### Task 4.1: Extract layers for `sandbox.exec`

**File:** `autonoetic-gateway/src/runtime/tools.rs` (resolve artifact section)

- [ ] After resolving artifact files, also resolve layers
- [ ] For each layer, extract to temp dir via `layer_store.extract_to()`
- [ ] Add extraction dir as `SandboxMount` at declared `mount_path`

**Tests:**
- [ ] `sandbox.exec` with layered artifact mounts files + layer
- [ ] Layer mount path is accessible inside sandbox
- [ ] Files in layer are readable inside sandbox
- [ ] Layer files don't conflict with artifact flat files

### Task 4.2: Update `artifact.inspect`

- [ ] Return `layers` field in inspect response so agents know what layers an artifact has

---

## Phase 5: Builder Agent

### Task 5.1: Create builder agent directory

**File:** `agents/specialists/builder.default/`

- [ ] Create `SKILL.md` with:
  - Role: build-time dependency resolution and artifact layering
  - Capabilities: `SandboxFunctions` (content., artifact., sandbox.exec), `ReadAccess`, `WriteAccess`, `CodeExecution`
  - Instructions: read requirements.txt, install deps via sandbox.exec with capture_paths, build artifact with layers
- [ ] Create `runtime.lock` with empty dependencies
- [ ] Create `sandbox.conf` with `share_net = true`

### Task 5.2: Update planner routing

**File:** `agents/lead/planner.default/SKILL.md`

- [ ] Add builder to delegation ladder between coder and evaluator
- [ ] Rule: "if artifact has a dependency file (requirements.txt, package.json, etc.), delegate to builder before evaluator"
- [ ] Builder receives artifact ID + dependency file name, returns layered artifact ID

### Task 5.3: Update evaluator instructions

**File:** `agents/specialists/evaluator.default/SKILL.md`

- [ ] Add instruction: "never try to install packages manually — deps should be in the artifact layers"
- [ ] Add instruction: "use PYTHONPATH/NODE_PATH as declared in the artifact's layer mount_path"
- [ ] Add instruction: "if deps are missing, report 'artifact missing required layers' instead of trying to install"

---

## Phase 6: Integration Tests

### Task 6.1: Layer store integration test

**File:** `autonoetic-gateway/tests/layer_store_integration.rs`

- [ ] End-to-end: create temp dir with files → create layer → extract → verify
- [ ] Dedup test
- [ ] Size limit enforcement

### Task 6.2: Capture paths integration test

**File:** `autonoetic-gateway/tests/sandbox_capture_integration.rs`

- [ ] `sandbox.exec` writes files → capture_paths → verify captured layer
- [ ] `sandbox.exec` without capture_paths → no layers in response

### Task 6.3: Layered artifact integration test

**File:** `autonoetic-gateway/tests/layered_artifact_integration.rs`

- [ ] Build artifact with layers → inspect → verify layers in manifest
- [ ] Run `sandbox.exec` with layered artifact → verify layer mounted correctly
- [ ] Artifacts without layers still work identically

### Task 6.4: Full lifecycle integration test

**File:** `autonoetic-gateway/tests/build_layer_lifecycle_integration.rs`

- [ ] Simulate the full flow: content.write → sandbox.exec with capture → artifact.build with layers → sandbox.exec with artifact
- [ ] Verify the weather demo scenario no longer loops

---

## Phase 7: Documentation

### Task 7.1: Update ARCHITECTURE.md

- [ ] Add "Layer Store" section to System Components table
- [ ] Add "Build Layer Flow" to Data Flow section
- [ ] Update "Content Storage" section to mention layers

### Task 7.2: Update AGENTS.md

- [ ] Add builder to roles table
- [ ] Add builder to delegation ladder
- [ ] Document the `capture_paths` sandbox.exec parameter

### Task 7.3: Update content-store.md or create layers.md

- [ ] Document layer storage layout
- [ ] Document `capture_paths` semantics
- [ ] Document layer-aware artifact.build
- [ ] Document sandbox.conf `share_net` option

### Task 7.4: Update CLI.md

- [ ] Document any new CLI commands for layer inspection (if added)

---

## Execution Order & Dependencies

```
Phase 1 (types + store)     ← no dependencies, start here
  ├── 1.2 LayerManifest types
  ├── 1.3 ArtifactBundle update
  └── 1.1 LayerStore module

Phase 2 (capture_paths)     ← depends on 1.1, 1.2
  ├── 2.1 Extend SandboxExecArgs
  ├── 2.2 Implement capture logic
  └── 2.3 Tool definition

Phase 3 (layered artifact.build) ← depends on 1.1, 1.3
  ├── 3.1 Accept layers in build()
  └── 3.2 Tool definition

Phase 4 (sandbox mounting)  ← depends on 1.1, 3.1
  ├── 4.1 Extract + mount layers
  └── 4.2 Update inspect

Phase 5 (agents)            ← depends on 2, 3, 4 being complete
  ├── 5.1 Builder agent
  ├── 5.2 Planner routing update
  └── 5.3 Evaluator instructions

Phase 6 (integration tests) ← depends on all above
  ├── 6.1 Layer store
  ├── 6.2 Capture paths
  ├── 6.3 Layered artifacts
  └── 6.4 Full lifecycle

Phase 7 (docs)              ← can start after Phase 1, finalize after Phase 6
```

**Estimated tasks:** 24
**Critical path:** 1.2 → 1.1 → 2.2 → 5.1 → 6.4
