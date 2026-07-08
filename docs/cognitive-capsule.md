# Cognitive Capsules

> **Reference** — Describes the implemented capsule export/import pipeline
> (`autonoetic-gateway/src/capsule/`, CLI `autonoetic capsule *`).
> Historical implementation plan:
> [`archived/cognitive-capsule-implementation-plan.md`](archived/cognitive-capsule-implementation-plan.md).

**Concept origin**: [`archived/concepts.md`](archived/concepts.md) (line 41)

## Background

A **Cognitive Capsule** is:

> *"A portable export that wraps the Agent bundle plus its runtime closure: `runtime.lock`, artifact references or embedded cached artifacts, and optionally the exact Gateway binary required to relaunch the same autonomous behavior somewhere else."*

The pipeline is implemented on top of:
- **Immutable agent revisions** (`AgentRevisionRecord`, `AgentRef`, `AgentAliasRecord`) with content-addressed digests and Ed25519 signatures
- **Build layers** (`LayerStore`, `LayerManifest`, `CapturedLayer`) for dependency bundles
- **Content-addressed artifact store** (`ArtifactBundle`, `ArtifactFileEntry`, `ArtifactHandle`)
- **RuntimeLock** with `LockedLayerMount` for full execution closure pinning
- **OFP federation** for cross-gateway wire protocol
- **Redaction infrastructure** for secret scrubbing
- **Checkpoint/resume** for session state serialization

These are the building blocks that make capsule export/import practical without inventing new subsystems.

---

## What a Cognitive Capsule IS

A Cognitive Capsule is a **self-contained, portable snapshot** of an agent at a specific point in time. It captures everything needed to reproduce that agent's behavior on a different machine, a different gateway, or in a different environment.

### Core Properties

| Property | Description |
|---|---|
| **Immutable** | Once created, a capsule never changes (content-addressed archive) |
| **Self-describing** | Contains a `capsule.json` manifest with everything a receiving gateway needs to validate and import |
| **Revision-pinned** | Captures a specific `AgentRevisionRecord`, not a mutable alias |
| **Closure-complete** | Includes or references all dependencies: `runtime.lock`, layers, artifacts, skills |
| **Secret-free** | Secrets are scrubbed before export; the receiving gateway's vault provides its own credentials |
| **Signed** | Optionally Ed25519-signed for authenticity and tamper detection |
| **Portable** | No Autonoetic-specific binary formats; the manifest is JSON, agent instructions are Markdown, skills are text |

### What It Is NOT

- **Not live migration** — a capsule is a snapshot, not a running process transfer
- **Not a backup** — it contains no session history or checkpoint state by default (opt-in via mode)
- **Not a container image** — it's lighter; layers are mounted into sandboxes, not booted as VMs

---

## Use Cases

1. **Environment transfer** — Move a tuned agent from a dev machine to a production gateway
2. **Cloud projection** — Export a local agent and import it on a cloud-hosted gateway for burst compute
3. **Marketplace/sharing** — ZIP a "Senior Python Coder" agent and share it; recipient imports it and boots immediately
4. **Disaster recovery** — Export critical agents periodically; restore from capsule after data loss
5. **Hermetic replay** — Bundle the exact gateway binary + runtime closure for bit-identical replay of past behavior
6. **Federation seeding** — Send a capsule to a federated peer gateway via OFP for remote execution
7. **Team onboarding** — Share pre-trained agents (with accumulated memory and skills) across team members

---

## Capsule Modes

Building on the existing `CapsuleMode::Thin | Hermetic` enum:

| Mode | What's included | Use case |
|---|---|---|
| **Thin** | Agent revision + `runtime.lock` + artifact/layer **references** | Fast export; receiving gateway fetches deps from marketplace/network |
| **Hermetic** | Agent revision + `runtime.lock` + embedded artifact **content** + layers + optionally gateway binary | Offline/air-gapped replay; no network needed on import |
| **Replay** | Hermetic + session checkpoint + compressed context capsule (Phase 2 `StateCapsule`) | Resume an agent session exactly where it left off |
| **Headless** | Thin or Hermetic + scheduled job definitions | Re-create cold path (cron) agents that run without human interaction |

> **Note**: **Replay mode** is new and depends on the checkpoint/session-resume infrastructure. It bundles `capsule_state` (from the Phase 2 CapsuleStrategy RFC) plus the session checkpoint. The receiving gateway can resume the session mid-turn. This is the most ambitious mode and should ship after Thin + Hermetic are stable.

---

## Open Questions

1. **Memory inclusion**: Should capsules include durable memory (`knowledge_store` entries with scope `memory` and `user_profile`)? Including memory makes the capsule more useful (agent remembers learned facts), but memory may contain PII or project-specific facts that shouldn't leave the origin gateway. Recommendation: memory export is **opt-in** (`--include-memory` flag) and the memory snapshot is run through the redaction pipeline before inclusion.

2. **Signature trust model**: The existing revision auto-sign uses gateway-derived Ed25519 keys. For capsules crossing trust boundaries, we need a trust model: should the importing gateway accept any signature? Only from trusted signers listed in config? Or prompt the operator for approval? Recommendation: configurable `trusted_capsule_signers` list in `config.yaml`, similar to `trusted_signers` for constitutions.

3. **Cross-version compatibility**: Should a capsule exported from gateway v0.3.0 be importable on v0.2.0? Recommendation: forward-compatible only — older gateways reject capsules with `format_version` they don't understand. Newer gateways import older formats.

4. **Layer portability**: Layers contain platform-specific binaries (`.so`, `.dll`). Should the capsule manifest declare the build platform so the receiving gateway can reject incompatible layers? Recommendation: add `platform: {os, arch}` to `LayerManifest` and validate at import time.

5. **Causal chain inclusion**: Should capsules optionally include the causal chain log? It's immutable and useful for audit, but can be very large. Recommendation: opt-in via `--include-causal-chain` flag, with size cap.

---

## Existing Code to Reuse (Zero Changes Needed)

| Component | File | What it provides | Used for |
|---|---|---|---|
| CapsuleManifest | `autonoetic-types/src/capsule.rs` | `CapsuleManifest`, `CapsuleMode`, `IncludedArtifact`, `CapsuleGatewayRuntime` | Base schema (extended) |
| RuntimeLock | `autonoetic-types/src/runtime_lock.rs` | `RuntimeLock`, `LockedLayerMount`, `LockedArtifact` | Execution closure pinning |
| AgentRevision | `autonoetic-types/src/agent_revision.rs` | `AgentRevisionRecord`, `AgentRef`, `short_id()` | Immutable revision identity |
| Artifact | `autonoetic-types/src/artifact.rs` | `ArtifactBundle`, `ArtifactHandle`, `ArtifactKind::AgentBundle` | Artifact closure |
| Layer | `autonoetic-types/src/layer.rs` | `LayerManifest`, `ArtifactLayer`, `CapturedLayer` | Dependency bundles |
| Redaction | `autonoetic-types/src/redaction.rs` | `redact_json_value()`, `redact_embedded_secrets()`, `is_sensitive_key()` | Secret scrubbing at export |
| ContentStore | `autonoetic-gateway/src/runtime/content_store.rs` | Content-addressed blob store with SHA-256 handles | File storage/dedup |
| Checkpoint | `autonoetic-gateway/src/runtime/checkpoint.rs` | `SessionCheckpoint`, `restore_into()` | Session state for Replay mode |

---

## Proposed Changes

### CapsuleManifest Schema Extension

**File**: `autonoetic-types/src/capsule.rs`

```rust
/// Capsule mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleMode {
    Thin,
    Hermetic,
    Replay,
    Headless,
}

/// Cryptographic signature for capsule integrity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleSignature {
    /// Algorithm: "ed25519"
    pub algorithm: String,
    /// Signer identity: "gateway:{fingerprint}", "user:{id}", "ci:{pipeline}"
    pub signer_id: String,
    /// Base64-encoded signature over capsule_content_digest.
    pub signature: String,
}

/// Memory snapshot included in the capsule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleMemorySnapshot {
    /// Number of memory entries included.
    pub entry_count: usize,
    /// Scopes included (e.g., ["memory", "user_profile"]).
    pub scopes: Vec<String>,
    /// Content handle for the memory dump file in the capsule archive.
    pub content_handle: String,
    /// Whether redaction was applied before export.
    pub redacted: bool,
}

/// Provenance record — where this capsule came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleProvenance {
    /// Gateway node ID that created this capsule.
    pub origin_node_id: String,
    /// Gateway version at export time.
    pub gateway_version: String,
    /// Trust domain: "local", "partner", "foreign".
    pub trust_domain: String,
    /// Previous capsule ID if this is a re-export of an imported capsule.
    pub parent_capsule_id: Option<String>,
}

/// Layer reference embedded in the capsule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleLayerRef {
    pub layer_id: String,
    pub name: String,
    pub digest: String,
    pub size_bytes: u64,
    /// If hermetic: content handle for the embedded layer archive.
    /// If thin: None (receiving gateway must fetch/build the layer).
    pub embedded_handle: Option<String>,
    /// Build platform for compatibility checks.
    pub platform: Option<CapsulePlatform>,
}

/// Platform descriptor for layer compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsulePlatform {
    pub os: String,     // "linux", "macos", "windows"
    pub arch: String,   // "x86_64", "aarch64"
}

/// The `capsule.json` manifest — extended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleManifest {
    // --- Identity ---
    pub capsule_id: String,
    pub format_version: String,  // "1.0.0"
    pub mode: CapsuleMode,
    pub created_at: String,

    // --- Agent identity ---
    pub agent_id: String,
    /// Pinned revision ID (immutable, content-addressed).
    pub revision_id: String,
    /// Short ID for human reference.
    pub revision_short_id: String,
    /// Content digest of the revision.
    pub content_digest: String,

    // --- Execution closure ---
    pub entrypoint: String,   // "SKILL.md"
    pub runtime_lock: String, // "runtime.lock"

    // --- Included content ---
    #[serde(default)]
    pub included_artifacts: Vec<IncludedArtifact>,
    #[serde(default)]
    pub included_layers: Vec<CapsuleLayerRef>,
    #[serde(default)]
    pub included_skills: Vec<String>,

    // --- Optional sections ---
    pub gateway_runtime: Option<CapsuleGatewayRuntime>,
    pub memory_snapshot: Option<CapsuleMemorySnapshot>,
    /// Checkpoint handle for Replay mode.
    pub checkpoint_handle: Option<String>,

    // --- Security ---
    #[serde(default)]
    pub redactions: Vec<String>,
    pub signature: Option<CapsuleSignature>,
    pub provenance: CapsuleProvenance,

    // --- Dependencies ---
    /// Required agents that should exist on the receiving gateway.
    #[serde(default)]
    pub requires_agents: Vec<String>,
    /// Required skills that should exist on the receiving gateway.
    #[serde(default)]
    pub requires_skills: Vec<String>,
}
```

**Key changes from current**:
- `revision_id` + `content_digest` — pins to immutable revision, not mutable alias
- `included_layers` — references dependency bundles (from the build layer system)
- `memory_snapshot` — opt-in memory export with redaction
- `checkpoint_handle` — Replay mode session state
- `signature` — Ed25519 signing
- `provenance` — origin tracking for federation chains
- `format_version` — forward compatibility
- `requires_agents/skills` — dependency declaration for import validation

---

### Export Pipeline

**File**: `autonoetic-gateway/src/capsule/export.rs` (new)

The export pipeline:

1. **Resolve revision** — load `AgentRevisionRecord` by agent_id + optional revision selector
2. **Collect files** — SKILL.md, runtime.lock, all files in the revision's content store
3. **Collect artifacts** — resolve artifact references from the revision
4. **Collect layers** — if hermetic, fetch and embed layer archives from `LayerStore`
5. **Collect memory** — if `--include-memory`, query `knowledge_store` for scoped entries, apply `redact_json_value()`
6. **Collect checkpoint** — if Replay mode, serialize `SessionCheckpoint`
7. **Scrub secrets** — apply `redact_embedded_secrets()` to all text content; strip env vars from runtime.lock
8. **Build manifest** — assemble `CapsuleManifest`
9. **Compute digest** — SHA-256 over canonical manifest JSON
10. **Sign** — if `--sign`, Ed25519 sign the digest
11. **Archive** — tar.zst the entire capsule directory
12. **Emit causal event** — `capsule_export` event to the causal chain

#### Capsule Archive Structure

```
capsule_<id>/
├── capsule.json          # CapsuleManifest
├── agent/
│   ├── SKILL.md          # Agent manifest
│   ├── runtime.lock      # Execution closure
│   └── files/            # Content store files (source code, configs)
├── artifacts/            # (hermetic only) artifact bundles
│   └── art_<id>/
│       └── manifest.json
├── layers/               # (hermetic only) layer archives
│   └── layer_<hash>/
│       └── contents.tar.zst
├── memory/               # (opt-in) memory snapshot
│   └── memory_snapshot.json
└── checkpoint/           # (replay only) session checkpoint
    └── checkpoint.json
```

---

### Import Pipeline

**File**: `autonoetic-gateway/src/capsule/import.rs` (new)

The import pipeline:

1. **Extract archive** — unpack tar.zst to temp directory
2. **Parse manifest** — deserialize `capsule.json`, validate `format_version`
3. **Verify signature** — if signature present and `trusted_capsule_signers` configured, verify Ed25519
4. **Check compatibility** — validate `runtime_lock` against local gateway version, check layer platform compatibility
5. **Check dependencies** — warn if `requires_agents` or `requires_skills` are missing locally
6. **Dedup artifacts** — for each `IncludedArtifact`, check content store by digest; skip if already present
7. **Dedup layers** — for each `CapsuleLayerRef`, check `LayerStore` by digest; skip if already present
8. **Create revision** — insert `AgentRevisionRecord` with `source_kind: "capsule_import"`, `source_ref: capsule_id`
9. **Import memory** — if `memory_snapshot` present, merge into local `knowledge_store` (skip duplicates by key)
10. **Import checkpoint** — if Replay mode, store checkpoint for session resume
11. **Optionally promote** — if `--activate`, create alias binding pointing to the imported revision
12. **Emit causal event** — `capsule_import` event

---

### CLI Commands

**File**: `autonoetic/src/cli/capsule.rs` (new)

```
autonoetic capsule export <agent_id> [options]
  --mode thin|hermetic|replay|headless    (default: thin)
  --revision <revision_selector>          (default: current alias target)
  --include-memory                        (include memory snapshot, redacted)
  --include-causal-chain                  (include causal chain log)
  --sign                                  (sign with gateway key)
  --output <path>                         (default: <agent_id>.capsule.tar.zst)

autonoetic capsule import <capsule_path> [options]
  --verify-signature                      (verify signature before import)
  --activate                              (promote imported revision to alias)
  --dry-run                               (validate without importing)
  --trust-domain <domain>                 (override trust domain: local|partner|foreign)

autonoetic capsule verify <capsule_path>
  Validates manifest schema, artifact/layer digests, signature, runtime.lock
  compatibility. Prints a verification report.

autonoetic capsule inspect <capsule_path>
  Prints manifest summary: agent_id, revision, mode, included artifacts/layers,
  memory snapshot presence, signature status, provenance chain.
```

---

### Gateway Tools (Agent-Initiated)

**File**: `autonoetic-gateway/src/runtime/tools/capsule.rs` (new)

```rust
/// capsule.export — export an agent as a capsule (agent-initiated)
/// Capability: CapsuleExport (any agent_id) OR SelfCapsuleExport (own agent_id only, Ri-0.17)
capsule_export(agent_id, mode?, include_memory?) → { capsule_path, capsule_id }

/// capsule.import — import a capsule (agent-initiated)
/// Capability: CapsuleExport
capsule_import(capsule_path, activate?) → { agent_id, revision_id }
```

These are gated by a `CapsuleExport` capability variant in `capability.rs`.
`capsule.export` additionally accepts the scoped `SelfCapsuleExport` variant
(Ri-0.17: emigration), which restricts export to the caller's own `agent_id`
(`manifest.agent.id`) via `policy.rs::can_use_capsule_self`. The broad
`CapsuleExport` remains the operator-grant path for exporting any agent;
`capsule.import` is gated by `CapsuleExport` only.

---

### Configuration

**File**: `config/config-template.yaml`

```yaml
capsule:
  # Trusted signers for capsule import verification.
  # Format: "gateway:<fingerprint>" or "user:<id>"
  trusted_signers: []
  # Default export mode.
  default_mode: "thin"
  # Maximum capsule archive size (bytes). Default 2GB.
  max_capsule_size_bytes: 2147483648
  # Whether to auto-sign exported capsules with the gateway key.
  auto_sign: true
  # Whether to include memory by default.
  include_memory_by_default: false
```

---

### Capability Addition

**File**: `autonoetic-types/src/capability.rs`

Add `CapsuleExport` variant:

```rust
pub enum Capability {
    // ...existing...
    CapsuleExport,
}
```

Policy gate in `policy.rs`: only agents with `CapsuleExport` can call `capsule.export` / `capsule.import`.

---

### Federation Integration (Future)

Capsules are the natural unit for cross-gateway agent transfer via OFP. The OFP `RouteMessage` method can carry capsule metadata; the actual archive transfers via a new `CapsuleTransfer` extension method:

```rust
// OFP extension (negotiated at handshake)
"capsule_transfer":
  CapsuleOffer { capsule_id, manifest_digest, size_bytes }
  CapsuleAccept { capsule_id }
  CapsuleData { capsule_id, chunk_index, data: Vec<u8> }
  CapsuleComplete { capsule_id, digest }
```

This is Phase 4 — ships after the CLI export/import pipeline is stable.

---

## Implementation Phases

### Phase 1: Schema & Core Types
- Extend `CapsuleManifest` in `capsule.rs`
- Add `CapsuleSignature`, `CapsuleProvenance`, `CapsuleLayerRef`, `CapsuleMemorySnapshot`, `CapsulePlatform`
- Add `CapsuleExport` capability variant
- Add capsule config section
- Unit tests: manifest serialization roundtrip, format_version validation

### Phase 2: Export/Import Pipeline
- `autonoetic-gateway/src/capsule/export.rs` — full export pipeline
- `autonoetic-gateway/src/capsule/import.rs` — full import pipeline
- `autonoetic-gateway/src/capsule/verify.rs` — signature + digest verification
- Secret scrubbing integration with `redaction.rs`
- Layer/artifact deduplication on import
- Integration tests: export→import roundtrip, hermetic mode with layers, thin mode with references, secret scrubbing verification, revision creation on import

### Phase 3: CLI & Gateway Tools
- `autonoetic/src/cli/capsule.rs` — `export`, `import`, `verify`, `inspect` subcommands
- `autonoetic-gateway/src/runtime/tools/capsule.rs` — agent-initiated `capsule.export` / `capsule.import`
- Policy gate for `CapsuleExport` capability
- Integration tests: CLI roundtrip, agent-initiated export/import, dry-run mode

### Phase 4: Federation & Advanced Modes
- OFP `capsule_transfer` extension
- Replay mode (checkpoint + session resume on import)
- Headless mode (scheduled job definitions)
- Memory import with dedup/merge
- Cross-gateway capsule transfer test

---

## Verification Plan

### Automated Tests

1. **Manifest roundtrip**: serialize → deserialize → fields match, for all modes
2. **Export produces valid archive**: export thin/hermetic → verify archive structure matches spec
3. **Import creates correct revision**: import → `AgentRevisionRecord` exists with `source_kind: "capsule_import"`
4. **Deduplication**: export → import → export same agent again → import → no duplicate blobs in content store
5. **Secret scrubbing**: export agent with secrets in SKILL.md → verify no `sk-`, `Bearer`, `PASSWORD=` in archive
6. **Signature verification**: sign capsule → verify passes; tamper archive → verify fails
7. **Layer compatibility**: export on x86_64 → import on aarch64 → layer platform mismatch warning
8. **Memory redaction**: export with `--include-memory` → verify memory entries have `redacted: true`, no raw secrets
9. **Replay mode**: export session checkpoint → import → session resumes from correct turn

### Manual Verification

- Export a real multi-agent planner setup, import on a fresh gateway, verify all agents boot and can delegate work
- Export a builder agent with dependency layers, import on a machine without those deps, verify sandbox execution works with embedded layers
