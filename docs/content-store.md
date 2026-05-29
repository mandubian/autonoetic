# Content Store

This document describes the content storage system with root-session visibility and artifact bundles.

## Overview

The content store provides **content-addressable storage** (SHA-256 based) for agent artifacts. Content is organized by a root-session visibility model where sessions sharing a root can collaborate.

## Architecture

```
.gateway/content/sha256/     ← Immutable content blobs (shared)
└── ab/c123...               ← Content indexed by hash

.gateway/sessions/
├── demo-session/            ← Root session manifest + reports
│   ├── manifest.json
│   ├── session_report.json  ← Structured session report (JSON)
│   ├── session_report.md    ← Human-readable report (markdown)
│   └── session_report.html  ← HTML report
└── demo-session/coder-abc123/  ← Child session manifest
    └── manifest.json

.gateway/artifacts/          ← Immutable artifact bundles
├── index.json
└── art_a1b2c3d4/            ← Gateway-internal storage dir (not agent-facing)
    └── manifest.json
```

### Key Concepts

| Concept | Description |
|---------|-------------|
| **Content Handle** | SHA-256 hash prefixed with `sha256:` |
| **Short Alias** | 8 hex chars for LLM-friendly lookup |
| **Session Manifest** | Maps names/handles to content with visibility |
| **Root Session ID** | Top-level session for visibility grouping |
| **Artifact** | Immutable file bundle for review/install/execution |
| **Published Report** | Session report stored in content store, registered in `published_session_reports` catalog for cross-session discovery via `observability_search`/`observability_read` |

## Visibility Model

### Three Visibility Levels

| Visibility | Scope | Default |
|-----------|-------|---------|
| `private` | Only the writing session | No |
| `session` | All sessions under same root_session_id | **Yes** |
| `global` | Cross-session durable | No |

### Root Session

The root session is the portion before the first `/` in a session ID:

- `"demo-session"` → root is `"demo-session"`
- `"demo-session/coder-abc123"` → root is `"demo-session"`
- `"demo-session/coder-abc123/specialist"` → root is `"demo-session"`

All sessions sharing the same root can read each other's `session`-visible content.

### Visibility Behavior

```
Root: demo-session
├── Planner (demo-session)          writes weather.py (session) → visible to all
├── Coder (demo-session/coder-abc)  writes draft.py (private)   → only coder sees it
└── Evaluator (demo-session/eval-1) can read weather.py          → session visibility
                                   cannot read draft.py          → private
```

## API Reference

### `content_write`

Write content with visibility control.

```json
// Request
{
  "name": "src/main.py",
  "content": "print('hello')",
  "visibility": "session"
}

// Response
{
  "ok": true,
  "ref": "cnt_a1b2c3d4",
  "alias": "a1b2c3d4",
  "name": "src/main.py",
  "sandbox_path": "/tmp/src/main.py",
  "visibility": "session"
}
```

Default visibility is `session` (collaborative). Use `private` for scratchpads/drafts.
Use `sandbox_path` when passing files to `sandbox_exec`.
`cnt_...` and `sha256:...` are content references for `content_read`, not filesystem paths.

### `content_read`

Read by name, handle, or alias with root-based resolution.

```json
// Request
{
  "name_or_handle": "main.py"
}

// Response
{
  "ok": true,
  "content": "print('hello')"
}
```

Resolution order:
1. If `ar.<ref>:<filename>` → read that file directly from the artifact bundle after scope resolution
2. If `cnt_<8 hex>` (or `cnt:<8 hex>`) → alias lookup (session, then root, then global)
3. If `sha256:...` → direct content lookup (with `sha256:<8 hex>` treated as alias fallback)
4. If bare 8 hex chars → alias lookup (session, then root, then global)
5. Otherwise → name lookup (session, then root, then global)

### `content_read` vs `artifact_inspect`

These tools intentionally overlap a bit, but they serve different jobs:

- `artifact_inspect` is for **artifact structure and trust-boundary review**: file list, entrypoints, layers, digest, builder metadata.
- `content_read` is for **retrieving bytes/text**: either session content (`name`, alias, handle) or a specific artifact file (`ar.<ref>:<filename>`).

Why this is usually not an issue:

- The trust boundary remains artifact-centric: review/install/execution flows key off the immutable manifest (`artifact_manifest_digest`) and canonical closure digest (`artifact_canonical_digest`).
- `content_read` does not replace structural review metadata (entrypoints/layers/provenance); it only fetches file content.
- Runtime rejects implicit workflow IDs for artifact-only operations (`artifact_inspect`, `artifact_exec`, `artifact_prepare`), preserving boundary clarity.

Practical guidance:

- Use `artifact_inspect` first when validating what an artifact contains.
- Use `content_read` second to open exact files returned by inspection.

### `artifact_build`

Build an immutable artifact bundle from session content.

```json
// Request
{
  "inputs": ["src/main.py", "src/utils.py"],
  "entrypoints": ["src/main.py"]
}
```

Accepted `inputs` forms:

| Tool field | Accepts | Does not accept |
|---|---|---|
| `artifact_build.inputs[]` | session content names, `cnt_...`, `sha256:...`, bare alias, existing artifact refs `ar.*`, canonical artifact IDs `art_*` | scoped artifact file refs like `ar.<ref>:requirements.txt` |
| `content_read.name_or_handle` | session content names, `cnt_...`, `sha256:...`, bare alias, scoped artifact file refs `ar.<ref>:<filename>` | bare artifact refs when you want structure/review metadata |
| `artifact_inspect.artifact_ref` | `ar.*`, `art_*` | `ar.<ref>:<filename>`, content handles |
| `artifact_prepare.artifact_ref` / `artifact_exec.artifact_ref` | `ar.*`, `art_*` | content handles, scoped artifact file refs |
| `artifact_resolve_ref.ref_id` | short scoped ref `ar.*` together with explicit `scope_type` and `scope_id` | `art_*`, `ar.<ref>:<filename>` |

Homogeneity rule:

- When a tool is artifact-oriented (`artifact_inspect`, `artifact_prepare`, `artifact_exec`, `artifact_build` artifact reuse), pass the artifact itself as `ar.*` or `art_*`.
- When a tool is file-oriented (`content_read`), pass a session content identifier or a scoped artifact file ref `ar.<ref>:<filename>`.
- Do not switch namespaces mid-flow: `ar.<ref>:<filename>` is for reading one file out of an artifact, while bare `ar.*` / `art_*` means the whole artifact.

```json
// Response
{
  "ok": true,
  "artifact_ref": "ar.a1b2c3d4ab12",
  "artifact_canonical_digest": "sha256:...",
  "artifact_manifest_digest": "sha256:...",
  "files": [
    {"name": "src/main.py", "handle": "sha256:...", "alias": "a1b2c3d4"},
    {"name": "src/utils.py", "handle": "sha256:...", "alias": "u5e6f7g8"}
  ],
  "entrypoints": ["src/main.py"],
  "created_at": "2026-03-19T..."
}
```

### `artifact_inspect`

Inspect an artifact by ref.

```json
// Request
{
  "artifact_ref": "ar.a1b2c3d4ab12"
}

// Response
{
  "ok": true,
  "artifact_ref": "ar.a1b2c3d4ab12",
  "artifact_canonical_digest": "sha256:...",
  "artifact_manifest_digest": "sha256:...",
  "files": [...],
  "entrypoints": [...],
  "created_at": "...",
  "builder_session_id": "..."
}
```

`artifact_inspect` accepts `artifact_ref` (`ar.<12-hex>`) returned by `artifact_build` or `workflow_wait`/`workflow_state`.
Implicit workflow output handles are content records and should be consumed via `content_read`.

## Artifact Trust Boundary

**Core rule: no artifact, no review / no install / no execution beyond scratch.**

Artifacts are the only units that may:
- Be reviewed by evaluator/auditor
- Be installed
- Be executed beyond scratch use
- Cross trust boundaries

The workflow for any executable-producing task:

1. Coder writes files via `content_write`
2. Coder builds an artifact: `artifact_build(inputs, entrypoints)`
3. Evaluator/auditor review the artifact via `artifact_inspect`
4. Install/run consumes only the `artifact_ref`

The artifact boundary must cover the full executable behavior surface, including:
- import and source resolution
- direct execution entrypoints
- runtime file-open/read/write access used by the executable

This closed-boundary rule applies equally to Python, shell, Node, generated scripts, config-driven runtimes, and similar executable file sets.

### Human-readable artifact projection

When an artifact is built, the gateway also materializes a read-only projection under:

```text
.gateway/sessions/<root-session-id>/artifacts/<artifact_id>/
```

(where `<artifact_id>` is the gateway-internal locator derived from the canonical digest; the agent-facing handle is always `artifact_ref`.)

On Unix hosts, the named files are symlinks to the canonical immutable content blobs, so the readable session view does not duplicate file contents. On other hosts, the gateway falls back to a best-effort non-duplicating link and only copies when linking is unavailable. The canonical trust boundary remains the immutable artifact manifest plus content handles.

## Manifest Structure

```json
{
  "names": {
    "weather.py": "sha256:abc123..."
  },
  "aliases": {
    "a1b2c3d4": "sha256:abc123..."
  },
  "root_session_id": "demo-session",
  "visibility": {
    "sha256:abc123...": "session"
  }
}
```

## Examples

### Planner Spawning Coder

```json
// Planner spawns coder
agent_spawn({"agent_id": "coder.default", "message": "Write weather.py"})

// Coder writes content (session visibility by default)
content_write({"name": "weather.py", "content": "import json..."})

// Planner can read the coder's output
content_read({"name_or_handle": "weather.py"})

// Coder builds artifact for review
artifact_build({"inputs": ["weather.py"], "entrypoints": ["weather.py"]})

// Evaluator reviews the artifact
artifact_inspect({"artifact_ref": "ar.a1b2c3d4ab12"})
```

### Private Scratch Work

```json
// Coder writes private draft (not visible to root/siblings)
content_write({"name": "draft.py", "content": "# scratch work", "visibility": "private"})

// Only the coder can read it
content_read({"name_or_handle": "draft.py"})  // works in coder session
// content_read in parent session → error
```

## Testing

```bash
cargo test --lib content_store
cargo test --lib artifact_store
cargo test --test content_storage_integration
```

Key test cases:
- `test_root_session_visibility` — parent reads child's session-visible content
- `test_private_visibility_isolates_from_root` — private content not visible to root
- `test_sibling_session_visibility` — siblings see each other's session content
- `test_artifact_build_and_inspect` — artifact lifecycle
