# Cognitive Capsule Standardization — Phased Implementation Plan

**Umbrella**: [#220](https://github.com/mandubian/autonoetic/issues/220)
**Companion design doc**: [`cognitive-capsule-standardization.md`](cognitive-capsule-standardization.md)

This document records the phased implementation strategy that landed the
Cognitive Capsule standardization umbrella. The companion design doc
covers the *what* and *why*; this one covers the *how* — what was split
out per phase, what was reused from existing infrastructure, and where
each surface lives in the tree.

## Status

| Phase | Issue | PR | Surface |
|---|---|---|---|
| 1 — Schema & Core Types | [#222](https://github.com/mandubian/autonoetic/issues/222) | [#280](https://github.com/mandubian/autonoetic/pull/280) | `autonoetic-types` |
| 2 — Export / Import Pipeline | [#223](https://github.com/mandubian/autonoetic/issues/223) | [#281](https://github.com/mandubian/autonoetic/pull/281) | `autonoetic-gateway::capsule` |
| 3 — CLI & Gateway Tools | [#224](https://github.com/mandubian/autonoetic/issues/224) | [#282](https://github.com/mandubian/autonoetic/pull/282) | `autonoetic::cli::capsule`, `runtime::tools::capsule` |
| 4 — Federation & Advanced Modes | [#225](https://github.com/mandubian/autonoetic/issues/225) | [#283](https://github.com/mandubian/autonoetic/pull/283) | `autonoetic-ofp::wire`, gateway Replay / Headless / memory glue |

PRs are stacked: each targets the previous phase's branch and
auto-retargets to `main` as the predecessor merges. The umbrella stays
open until all four merge.

## Context

Issue [#220](https://github.com/mandubian/autonoetic/issues/220) promotes
the existing minimal `CapsuleManifest` (in
`autonoetic-types/src/capsule.rs`) into a first-class portable agent
format. Before this work the codebase had only the bones —
`CapsuleMode { Thin, Hermetic }`, `IncludedArtifact`,
`CapsuleGatewayRuntime` — with no pipeline, no signature, no CLI, no
agent tools, no federation transport.

## Reusable infrastructure (zero changes needed)

The companion design doc identifies these building blocks. The capsule
pipeline composes them rather than reinventing storage, signing, or
redaction.

| Component | Path | Used for |
|---|---|---|
| `AgentRevisionRecord`, `AgentRef`, `short_id()` | `autonoetic-types/src/agent_revision.rs` | Immutable revision pinning |
| `RuntimeLock`, `LockedLayerMount`, `LockedArtifact` | `autonoetic-types/src/runtime_lock.rs` | Execution closure |
| `LayerStore`, `LayerManifest`, `CapturedLayer` | `autonoetic-types/src/layer.rs` + gateway store | Layer dedup/import |
| `ArtifactBundle`, `ArtifactHandle`, `ArtifactKind` | `autonoetic-types/src/artifact.rs` | Artifact closure |
| `redact_json_value`, `redact_embedded_secrets`, `is_sensitive_key` | `autonoetic-types/src/redaction.rs` | Secret scrubbing |
| `ContentStore` (SHA-256, dedup) | `autonoetic-gateway/src/runtime/content_store.rs` | Blob storage |
| `SessionCheckpoint`, `save_checkpoint`, `load_latest_checkpoint` | `autonoetic-gateway/src/runtime/checkpoint.rs` | Replay mode |
| `GatewayIdentityKey` (Ed25519 sign / fingerprint) | `autonoetic-gateway/src/runtime/crypto.rs` | Capsule signing |
| Causal events + `causal_events` SQLite mirror | gateway runtime | `capsule.export` / `capsule.import` audit |
| Scheduled jobs store | `autonoetic-gateway/src/scheduler/gateway_store/scheduled_jobs.rs` | Headless mode |
| Memory store (`memory_list_ids_owned_by`, `memory_get_unrestricted`, `memory_upsert`) | `autonoetic-gateway/src/scheduler/gateway_store/memory.rs` | Memory snapshot |

---

## Phase 1 — Schema & Core Types ([#222](https://github.com/mandubian/autonoetic/issues/222))

**Branch**: `capsule/phase-1-schema`
**Surface**: `autonoetic-types` only. Touches gateway only via exhaustive
match arms for the new `Capability` variant.

### Files modified

- `autonoetic-types/src/capsule.rs` — full rewrite of the manifest module:
  - `CapsuleMode` gains `Replay` and `Headless` variants
  - Helpers `is_hermetic()`, `needs_checkpoint()` on `CapsuleMode`
  - New structs: `CapsuleSignature`, `CapsuleMemorySnapshot`, `CapsuleProvenance`, `CapsulePlatform`, `CapsuleLayerRef`
  - `CapsuleManifest` gains: `format_version` (const `CAPSULE_FORMAT_VERSION = "1.0.0"`), `revision_id`, `revision_short_id`, `content_digest`, `included_layers`, `included_skills`, `memory_snapshot`, `checkpoint_handle`, `signature`, `provenance`, `requires_agents`, `requires_skills`
  - All new fields use `#[serde(default)]` / `skip_serializing_if = "Option::is_none"` so older capsules still parse (forward compat)
  - Methods `format_major_version() -> Option<u64>` and `layer_embedded_path()`
- `autonoetic-types/src/capability.rs`:
  - New `Capability::CapsuleExport` variant (unit; no payload — capsule pipelines self-describe scope via mode/include flags)
  - Update `capability_type_name`, `all_capability_kind_names`, and the exhaustiveness pin test
- `autonoetic-types/src/config.rs`:
  - New `CapsuleConfig` struct: `trusted_signers: HashMap<String,String>`, `default_mode: String`, `max_capsule_size_bytes: u64`, `auto_sign: bool`, `include_memory_by_default: bool`
  - Wire `pub capsule: CapsuleConfig` into `GatewayConfig` with `#[serde(default)]`
- `autonoetic-gateway/src/runtime/analysis/mod.rs`, `runtime/capability_inference.rs`, `runtime/state_attestation.rs`, `runtime/tools/mod.rs`:
  - Add `Capability::CapsuleExport => "CapsuleExport"` arm to each local `capability_type_name` match (4 sites; compiler-enforced via E0004)
- `config/config-template.yaml` — commented `capsule:` block under retention
- `docs/config-reference.md` — appended "Cognitive Capsules" subsection mirroring the retention/sentinel sections

### Verification

```bash
cargo test -p autonoetic-types --lib capsule        # 11 new unit tests
cargo test -p autonoetic-types --lib capability     # 9 capability tests
cargo build --workspace
```

---

## Phase 2 — Export / Import Pipeline ([#223](https://github.com/mandubian/autonoetic/issues/223))

**Branch**: `capsule/phase-2-pipeline`
**Surface**: `autonoetic-gateway` — new module `capsule/`.

### Module layout (in `autonoetic-gateway/src/capsule/`)

```
mod.rs       — public re-exports: ExportRequest, ExportOutcome,
               ImportRequest, ImportOutcome, MemoryConflictPolicy,
               ExportContext, ImportContext
archive.rs   — tar.zst pack/unpack with size cap from
               CapsuleConfig.max_capsule_size_bytes + path-traversal
               guard (rejects absolute paths and `..` segments)
verify.rs    — canonical-JSON digest (signature field cleared during
               canonicalisation), Ed25519 sign_manifest /
               verify_signature, SignatureStatus
export.rs    — pipeline: resolve → collect → scrub → manifest → digest
               → sign → archive → event
import.rs    — pipeline: extract → parse → verify → compat → dedup →
               revision → memory → event
```

Wired into `autonoetic-gateway/src/lib.rs` (`pub mod capsule;`).

### Export pipeline (`export.rs`)

1. Resolve agent + revision selector via existing `GatewayStore::get_agent_revision` / `get_agent_alias`.
2. Copy SKILL.md, runtime.lock, and the rest of the revision directory under `agent/` in a tempdir.
3. Apply `redact_embedded_secrets()` per text file; record per-file redactions.
4. (Phase 4) Hermetic mode embeds layer archives; Replay mode bundles the latest `SessionCheckpoint`; Headless mode bundles scheduled jobs.
5. Build `CapsuleManifest`; populate `provenance` from gateway `node_id`, crate version, `"local"`.
6. If `signed`: load `GatewayIdentityKey` and sign the canonical JSON (signature field cleared during canonicalisation, mirroring `state_attestation`).
7. Write `capsule.json` to staging.
8. `archive::pack` the staging dir into `tar.zst`.
9. Enforce `cfg.max_capsule_size_bytes` on the resulting archive.
10. Emit a `capsule.export` causal event (`category = "capsule"`, `action = "export"`, payload `{capsule_id, revision_id, mode, size_bytes, signed}`).

### Import pipeline (`import.rs`)

1. Stat the archive; refuse if larger than `max_capsule_size_bytes`.
2. Extract into a tempdir via `archive::unpack`, which also enforces the
   size cap on the *decompressed* total.
3. Parse `capsule.json`; refuse `format_major_version` > current.
4. Verify signature against `cfg.trusted_signers` when
   `verify_signature` is true; refuse `Mismatch` /
   `UntrustedSigner` / `MissingRequired` outcomes.
5. (Phase 4) Platform compat: refuse when `trust_domain != "local"`
   and `CapsulePlatform` mismatches the local one.
6. Walk `agent/` and dedup blobs into `ContentStore`; track
   `dedup_savings_bytes`.
7. Materialise the revision directory at
   `.gateway/revisions/agents/<agent>/<rev>/`.
8. Insert `AgentRevisionRecord` with `source_kind = "capsule_import"` and
   `source_ref = capsule_id`.
9. If `--activate`: upsert the agent alias to point at the new revision.
10. (Phase 4) Memory + checkpoint + scheduled-jobs side effects.
11. Emit a `capsule.import` causal event.

### Tests

`autonoetic-gateway/tests/capsule_pipeline_integration.rs` covers:

- thin export → import roundtrip creates a `capsule_import` revision
- `SKILL.md` containing `sk-…` is scrubbed before the archive is written
- `--dry-run` leaves the receiver untouched
- tampered manifest fails signature verification
- archive over `max_capsule_size_bytes` is refused before extraction
- second import is a no-op for an already-present revision (non-decreasing dedup)

Unit tests live in each submodule (`archive`, `export`, `import`,
`verify`): 14 in total.

---

## Phase 3 — CLI & Gateway Tools ([#224](https://github.com/mandubian/autonoetic/issues/224))

**Branch**: `capsule/phase-3-cli`
**Surface**: `autonoetic` (CLI) + `autonoetic-gateway` (agent tools, policy).

### CLI (`autonoetic/src/cli/capsule.rs`)

Subcommands plumbed through clap, dispatched from `autonoetic/src/main.rs`:

```
autonoetic capsule export <agent_id>
  [--mode thin|hermetic|replay|headless] [--revision <rev_id>]
  [--include-memory] [--sign] [--output <path>]
  [--session-id <id>]          # required for --mode replay
  [--root-session-id <id>]     # required for --mode headless
  [--json]

autonoetic capsule import <path>
  [--verify-signature] [--activate] [--dry-run]
  [--trust-domain local|partner|foreign]
  [--memory-conflict keep-local|overwrite-local]
  [--json]

autonoetic capsule verify <path>  [--json]
autonoetic capsule inspect <path> [--json]
```

Each subcommand loads `config.yaml` via `autonoetic_gateway::config::load_config`, opens the `GatewayStore`, and delegates to `autonoetic_gateway::capsule::{export, import, verify}`.

`verify` prints a structured report (manifest schema, canonical digest,
signature status, redactions, provenance). `inspect` prints the
`capsule.json` summary.

### Agent tools (`autonoetic-gateway/src/runtime/tools/capsule.rs`)

```
capsule.export(agent_id, mode?, include_memory?, sign?, output?,
               revision?, session_id?, root_session_id?)
  → { capsule_path, capsule_id, mode, signed, size_bytes,
      manifest_digest, redactions }

capsule.import(archive, verify_signature?, activate?, dry_run?,
               trust_domain?)
  → { capsule_id, agent_id, revision_id, signature_status,
      created_revision, dedup_savings_bytes, … }
```

Both gated by `Capability::CapsuleExport`. Registered in
`runtime/tools/mod.rs::default_registry`.

### Policy

`PolicyEngine::can_use_capsule()` in `autonoetic-gateway/src/policy.rs`:
pattern-matches the `CapsuleExport` variant, returns `allow("P-1.1")` or
`deny("P-1.1")`. Tool execution checks this gate before unpacking
arguments.

### Tests

- `autonoetic-gateway/tests/capsule_tools_integration.rs` — capability gate (tool unavailable without `CapsuleExport`) + tool definition schema check
- `autonoetic/tests/capsule_cli_e2e.rs` — drives the actual binary end-to-end through export → inspect → verify → import --dry-run
- `docs/CLI.md` — new "Capsule Commands" section

---

## Phase 4 — Federation & Advanced Modes ([#225](https://github.com/mandubian/autonoetic/issues/225))

**Branch**: `capsule/phase-4-federation`
**Surface**: `autonoetic-ofp` (wire protocol), `autonoetic-gateway`
(transfer handlers, Replay / Headless / memory glue).

### OFP `capsule_transfer` extension (`autonoetic-ofp/src/wire.rs`)

New `WireRequest` variants:

```
CapsuleOffer    { capsule_id, manifest_digest, size_bytes }
CapsuleData     { capsule_id, chunk_index, data }   # base64 in JSON
CapsuleComplete { capsule_id, digest }
```

New `WireResponse` variants:

```
CapsuleAccept   { capsule_id }
CapsuleAck      { capsule_id, imported, reason, revision_id }
```

Plus the extension name constant
`CAPSULE_TRANSFER_EXTENSION = "capsule_transfer"` for handshake-time
negotiation. `autonoetic-ofp` picks up `base64 = "0.22"` for the
`CapsuleData.data` chunk encoding.

The receive-side dispatcher (`autonoetic-gateway/src/server/ofp.rs`) is
**deferred to a follow-up PR**. Until that lands, the existing
wildcard arm returns `WireResponse::Error { code: 501 }` for the new
variants. The wire schema is stable, so federation glue can be added
incrementally without re-opening the protocol.

### Schema additions (forward-compat)

In `autonoetic-types/src/capsule.rs`:

- `CapsuleManifest.scheduled_jobs: Vec<CapsuleScheduledJob>` — populated in Headless mode
- `CapsuleManifest.platform: Option<CapsulePlatform>` — stamped on every export

Both `#[serde(default)]`, so older capsules still parse.

### Replay mode

- Export takes a required `session_id` and bundles the latest
  `SessionCheckpoint` via `load_latest_checkpoint`, writing it to
  `checkpoint/checkpoint.json` and setting `checkpoint_handle`.
- Import deserialises the checkpoint and calls `save_checkpoint`, laying
  the file into the receiver's `.gateway/checkpoints/<session>/` tree.
  The scheduler's existing resume path picks it up on the next tick;
  no new JSON-RPC verb is needed.

### Headless mode

- Export takes a required `root_session_id` and bundles every row from
  `GatewayStore::list_scheduled_jobs_for_root` into `scheduled_jobs`.
- Import iterates `manifest.scheduled_jobs` and inserts each via
  `GatewayStore::create_scheduled_job`, prefixing the new `job_id`
  with `job_capsule_<original>_<uuid>` to avoid collisions with the
  receiver's pre-existing jobs.

### Memory snapshot (made real)

Phase 2 stubbed the snapshot; Phase 4 makes it real:

- Export: `memory_list_ids_owned_by` → loop `memory_get_unrestricted`
  → `redact_json_value` per entry → write `memory/memory_snapshot.json`.
- Import: walk entries, deserialise each `MemoryObject`, consult the
  conflict policy:

  ```
  enum MemoryConflictPolicy { KeepLocal, OverwriteLocal }   # default KeepLocal
  ```

  `KeepLocal` (the default) skips when a `memory_id` already exists
  locally; `OverwriteLocal` upserts unconditionally. The import outcome
  reports `memory_entries_imported` and `memory_entries_skipped`.

### Platform compatibility hard-fail

When `trust_domain != "local"` and the embedded `CapsulePlatform`
mismatches `std::env::consts::OS` / `ARCH`, the import is refused
before any persistence. Within the `"local"` trust domain the operator
explicitly bypasses the check (so dev workflows on macOS can pull
capsules built in Linux CI).

### Tests

- `autonoetic-ofp` unit tests — wire roundtrip for `CapsuleOffer`,
  `CapsuleData` (base64), `CapsuleAck`, plus the extension-name pin
- `autonoetic-gateway/tests/capsule_phase4_advanced_modes.rs`:
  - Replay export → import → checkpoint laid down at the receiver
  - Headless export → import → scheduled job recreated with prefixed ID
  - Memory KeepLocal vs OverwriteLocal policy behaviour
  - Platform mismatch refused in `foreign` trust domain, accepted in `local`

---

## Verification across all phases

After each phase merges:

```bash
cargo build --workspace
cargo test --workspace
cargo run -p autonoetic -- capsule export <fixture> --mode hermetic --sign   # Phase 3+
cargo run -p autonoetic -- capsule verify <archive>
cargo run -p autonoetic -- capsule import <archive> --activate --verify-signature
```

Causal-chain audit:

```bash
cargo run -p autonoetic -- trace list | grep capsule_export
cargo run -p autonoetic -- trace list | grep capsule_import
```

## Decisions confirmed during implementation

1. **`CapsuleConfig.auto_sign = true` default** — mirrors revision auto-signing; operators can override per-export via `--sign=false`.
2. **`dedup_savings_bytes` surfaced both in the causal event and the import outcome** — helps operators see the value in CLI output as well as audit history.
3. **Memory conflict policy default = `KeepLocal`** — avoids capsule imports silently overwriting curated knowledge on the receiver.
4. **OFP receive-side dispatcher deferred** — Phase 4 ships the wire schema and import glue; wiring the dispatcher (with HMAC + sequence-number integration) is a follow-up since the existing 501 fallback covers unknown variants safely.

## Deltas from the original plan

- The Phase 4 OFP **receive-side handler** in `server/ofp.rs` was
  intentionally deferred to a follow-up PR — the wire types alone are a
  stable surface and the existing wildcard error fallback covers any
  unknown variant.
- Memory conflict policy ships with two values (`KeepLocal`,
  `OverwriteLocal`) instead of the originally-sketched three; the
  `merge_into_namespaced_scope` policy was not necessary for the cases
  the test fixtures exercise and would have required schema-level
  decisions about scope namespacing that did not have a clear use case.
- Replay-mode session resume **does not** require a one-shot operator
  approval at this stage. The capsule pipeline only restores the
  checkpoint file; the scheduler's existing resume path retains its
  own approval/yield-reason gating, which already handles the
  cross-gateway case.
