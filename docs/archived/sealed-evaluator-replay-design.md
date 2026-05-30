> **ARCHIVED** — Historical design or implementation record. Not current source-of-truth. See [`docs/README.md`](../README.md) for live references.
>

**Status:** Draft
**Refs:** Issue #198, `docs/archived/promotion-federation-plan.md` §8, `docs/archived/recording-mode-design.md`, `agents/specialists/sealed_evaluator.default/SKILL.md`

---

## 1. Motivation

### 1.1 What exists today

Phase 2 (`--record-network`) lets the operator run the real agent and capture live HTTP traffic as redacted fixture files. The sealed evaluator (`sealed_evaluator.default`) already exists with `sandbox_network: sealed` and supports `fixture_set_ref` in its SKILL instructions. The recording proxy is proven.

What's missing: the ability to **point the sealed evaluator at a recorded fixture set** and get a deterministic replay verdict. Currently, the evaluator runs against whatever fixtures happen to be bundled with the artifact (which is usually none, since fixture authoring is what Phase 2 solves).

### 1.2 What we want

```
Operator: autonoetic eval sealed --artifact-ref ar_abc123 --fixture-set fs_xyz789
         ↓
1. Gateway loads FixtureSet metadata from SQLite
2. Locates fixture files from the recording staging directory
3. Copies fixtures into the artifact's fixture root
4. Spawns sealed_evaluator.default with fixture_set_ref in metadata
5. Evaluator runs artifact_exec → proxy serves recording fixtures
6. Evaluator returns deterministic verdict based on real recorded traffic
```

The operator gets a zero-effort replay: run `--record-network` once, then run `eval sealed` against the resulting fixture set any number of times, getting identical results each time (deterministic).

---

## 2. Design

### 2.1 Overview

Two paths for fixture replay:

**Path A — CLI-driven eval** (the primary path): `autonoetic eval sealed --artifact-ref X --fixture-set Y` is a top-level CLI command that creates a detached eval session. The gateway pre-populates the artifact sandbox with fixture files before the evaluator runs.

**Path B — SKILL-driven flow** (the agentic path): The sealed evaluator's SKILL.md already describes how to use `fixture_set_ref` in spawn metadata. This is the path the planner uses when orchestrating federation evaluation (the planner spawns the evaluator with fixture set metadata, the evaluator discovers the fixtures via `artifact_inspect`).

Phase 3 ships Path A. Path B is documented in the SKILL but its end-to-end integration (planner carrying `fixture_set_ref` → spawn metadata → evaluator) is deferred to Phase 4 or later.

### 2.2 Path A: CLI-driven eval

```
autonoetic eval sealed --artifact-ref ar_abc123 --fixture-set fs_xyz789
```

Flow:

```
1. CLI loads FixtureSet fs_xyz789 from SQLite
   └── FixtureSet.recording_session_id → look up RecordingSession
   └── RecordingSession has no direct file path stored in SQLite
       └── Convention: fixtures live at <gateway_dir>/recordings/<session_id>/fixtures/
           (where <session_id> is the RecordingSession.session_id)

2. CLI loads the artifact from ArtifactStore
   └── ArtifactStore.inspect(artifact_id) → ArtifactBundle
   └── Verifies the bundle is an AgentBundle kind

3. CLI creates a temp sandbox directory structure:
   <temp_dir>/
   ├── SKILL.md           (from artifact)
   ├── main.py            (from artifact)
   └── fixtures/          (copied from recording staging dir)
       ├── api.example.com/
       │   ├── GET-items.json
       │   └── POST-submit.json
       └── ...

4. CLI starts a headless agent session for sealed_evaluator.default
   └── Passes spawn metadata:
       {
         "fixture_set_ref": "fs_xyz789",
         "artifact_ref": "ar_abc123",
         "fixture_count": 15,
         "hosts": ["api.example.com", "auth.example.com"]
       }

5. Sealed evaluator runs artifact_exec against the artifact
   └── artifact_exec materializes files to temp_base
   └── temp_base already has fixtures/ copied in (or the gateway
       mounts them from the recording staging dir)
   └── Proxy serves recorded fixtures → deterministic execution

6. Evaluator returns {status: "passed"|"failed", evaluator_pass: bool, ...}
```

### 2.3 Fixture mounting strategy

The key question: **where do fixture files live, and how do they reach the sandbox?**

During recording (Phase 2), fixtures are written to:
```
<gateway_dir>/recordings/<recording_session_id>/fixtures/<host>/<METHOD>-<path>.json
```

During replay, the sealed proxy creates a `FixtureLoader` rooted at the artifact's temp directory (`temp_base`). The proxy expects fixtures at:
```
<temp_base>/fixtures/<host>/<METHOD>-<path>.json
```

**Approach:** Before the sandbox starts, copy or symlink the fixture files from the recording staging directory into the artifact's `temp_base/fixtures/` directory. This is a one-time filesystem operation.

For the CLI-driven eval:
```
1. Compute source:  <gateway_dir>/recordings/<session_id>/fixtures/
2. Create dest:     <temp_base>/fixtures/
3. Copy all files:  cp -r <source>/* <dest>/
```

For the SKILL-driven flow (Path B), the gateway does the same copy when it receives the `fixture_set_ref` in spawn metadata. The `agent_spawn` handler checks for `fixture_set_ref` in the metadata JSON, looks up the recording session, and pre-populates the artifact's fixture root before the sandbox starts.

### 2.4 Gateway integration for fixture mounting

**CLI eval command:** The command creates a `FixtureLoader` root directory with the artifact files + fixture files, then spawns the sealed evaluator as a headless agent run.

**agent_spawn fixture_set_ref:** When `agent_spawn` receives metadata containing a `fixture_set_ref`, the gateway:
1. Looks up the `FixtureSet` from SQLite
2. Resolves the recording session ID → staging directory path
3. Copies fixture files from the staging directory to the artifact's materialization directory
4. The proxy's `FixtureLoader` picks them up automatically

This requires modifying the `agent_spawn` handler to check for `fixture_set_ref` in metadata and perform the fixture copy. This is a small, focused change.

### 2.5 Types

No new types required. Phase 3 reuses:
- `FixtureSet` (from Phase 2) — metadata about recorded fixtures
- `RecordingSession` (from Phase 2) — the recording run that produced the fixtures
- `FixtureLoader` (pre-existing) — loads fixtures from disk
- `FixtureRecord` (from Phase 2) — the recorded HTTP round-trip format

The convention for resolving fixture files from a `FixtureSet`:
```rust
fn fixture_set_staging_dir(gateway_dir: &Path, session_id: &str) -> PathBuf {
    gateway_dir.join("recordings").join(session_id).join("fixtures")
}
```

### 2.6 CLI interface

```
autonoetic eval sealed --artifact-ref <ref> --fixture-set <id> [options]
```

| Flag | Required | Description |
|------|----------|-------------|
| `--artifact-ref` | Yes | Artifact ref to evaluate (ar_xxxxxxxx) |
| `--fixture-set` | Yes | Fixture set ID to replay (fs_xxxxxxxx) |
| `--agent-id` | No | Evaluator agent to use (default: sealed_evaluator.default) |
| `--json` | No | Output as JSON |
| `--timeout` | No | Max evaluation duration in seconds (default: 300) |

The command is added as a top-level subcommand:
```rust
pub enum Commands {
    // ... existing ...
    Eval(EvalArgs),
}
```

```rust
pub struct EvalArgs {
    #[command(subcommand)]
    pub command: EvalCommands,
}

pub enum EvalCommands {
    /// Run sealed evaluation against a recorded fixture set
    Sealed {
        /// Artifact ref to evaluate (ar_xxxxxxxx)
        #[arg(long)]
        artifact_ref: String,
        /// Fixture set ID to replay (fs_xxxxxxxx)
        #[arg(long)]
        fixture_set: String,
        /// Evaluator agent ID (default: sealed_evaluator.default)
        #[arg(long)]
        agent_id: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Max evaluation duration in seconds
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
}
```

---

## 3. Acceptance criteria

- [ ] `autonoetic eval sealed --artifact-ref X --fixture-set Y` CLI command
- [ ] Gateway copies fixture files from recording staging dir to artifact temp_base before sandbox starts
- [ ] Sealed evaluator runs deterministically against recorded fixtures
- [ ] `agent_spawn` with `fixture_set_ref` in metadata triggers fixture mounting
- [ ] Integration test: record a session with `--record-network`, then eval sealed against the resulting fixture set, verify deterministic replay
- [ ] Integration test: sealed evaluator with no fixture set still returns `unfixtured_target` (regression)
- [ ] CLI: `autonoetic eval sealed --help` works

---

## 4. Security & enforcement

### 4.1 Determinism guarantee

Replaying recorded fixtures is deterministic only for HTTP requests that match the recorded patterns. Any request to an unrecorded host/path returns `unfixtured_target` (502 error). This is the same behaviour as `Sealed` mode — the evaluator sees a structured error and can report "unable to evaluate" rather than failing.

### 4.2 Fixture integrity

Fixture files are written during recording and are immutable (content-addressed). The `FixtureSet` digest is a SHA-256 of the sorted file manifest. If the fixture files are tampered with between recording and replay, the evaluator's results may differ from the live-run outputs. This is acceptable for the diagnostic use case (operator-triggered evaluation); the operator is the trust root.

### 4.3 Recording-only data

Recorded fixtures may contain sensitive data that was intentionally captured during recording. The redaction layer (Phase 2) strips credentials before writing fixtures. The operator is responsible for ensuring the recording session's scope is appropriate for the data being captured.

---

## 5. Dependencies & boundaries

### 5.1 Dependencies

- Phase 2 recording mode (shipped)
- `sealed_evaluator.default` agent bundle (shipped)
- `FixtureSet` + `RecordingSession` types (shipped)
- Recording staging directory convention (shipped)
- `FixtureLoader` + proxy infrastructure (shipped)

### 5.2 Out of scope

- **Planner-driven federation eval** (the planner spawning sealed evaluator with fixture_set_ref) — the SKILL describes this but the planner integration is deferred
- **Fixture set versioning** — multiple fixture sets for the same revision are kept as separate records
- **Fixture set comparison** — diffing two fixture sets to detect behavioral changes between revisions
- **Automated fixture refresh** — re-recording when fixtures go stale

---

## 6. Open questions

1. **Fixture mounting responsibility**: Should the CLI command copy fixtures before spawning the evaluator, or should `artifact_exec` / `agent_spawn` do it on demand? **Proposed:** Both. The CLI copies fixtures upfront (simpler, no gateway code changes). The `agent_spawn` integration is the long-term path (requires gateway changes but works for all callers).

2. **Large fixture sets**: Copying 1000+ fixture files could be slow. Should we use symlinks instead of copies? **Proposed:** Symlinks when possible (same filesystem), copies otherwise. The temp_base is in `/tmp` and recording staging is under `gateway_dir` — likely different filesystems, so copies are the safe default.

3. **Evaluator output format**: The current sealed evaluator SKILL specifies `{status, evaluator_pass, summary}`. Should the eval CLI add a structured output wrapper? **Proposed:** The CLI wraps the evaluator output in a JSON envelope with fixture set metadata for traceability.
