# Spec: Artifact and Content Identity Model

## Purpose

Define a consistent, clean identity model for content and artifacts that works across:

- local single-gateway use
- multi-session and workflow-scoped handoff
- multi-node federation
- multi-tenant isolation
- LLM-facing tool ergonomics

Three distinct responsibilities that are currently conflated:

- canonical immutable identity
- local storage locator
- scoped agent-facing handle

These will be separated explicitly. There is no legacy constraint.

## Current State

### Content

- Canonical content identity is already strong: `sha256:<digest>`.
- Session manifests add two convenience layers:
  - registered names
  - short aliases (`8` hex chars) and `cnt_<alias>` refs
- Visibility is explicit: `private`, `session`, `global`.

This model is coherent:

- canonical identity = content digest
- convenience handles = names / aliases / refs

### Artifacts

- `artifact_id` is currently a short local identifier (`art_` + `8` hex chars).
- `artifact_digest` currently means the SHA-256 of the persisted manifest bytes.
- `artifact_ref` is a scoped short alias that maps to:
  - `artifact_id`
  - `artifact_digest`
  - scope metadata
  - expiry / revocation metadata

### Gateway / Node Identity

- Runtime writes `gateway.json`, containing build/version identity, not tenant identity and not stable instance identity.
- Federation/runtime provenance uses `config.node_id` / `AUTONOETIC_NODE_ID`.
- `node_id` is useful for provenance and namespacing, but it is not an authorization boundary and is not secret.

## Observations

### 1. `gateway.json` should not be used as a tenant identity

`gateway.json` contains build metadata:

- version
- build SHA
- binary SHA
- build tag

It identifies *what binary this gateway runs*, not *which tenant owns data*.

It is useful for:

- provenance
- diagnostics
- compatibility

It is not sufficient for:

- tenant partitioning
- ACL decisions
- artifact namespace isolation

### 2. `node_id` is useful, but only as provenance / namespace

`node_id` can be used for:

- provenance tagging
- federation origin metadata
- optional namespace prefixing for local locators

`node_id` should not be treated as:

- tenant identity
- a secret
- proof of authorization

### 3. `artifact_digest` is integrity-robust, but not canonical across tenants/nodes

Today the artifact manifest digest includes fields such as:

- `created_at`
- `builder_session_id`

Therefore two independent gateways building the same logical artifact closure will generally produce different `artifact_digest` values.

Current `artifact_digest` is therefore best understood as:

- a persisted-manifest integrity digest

not as:

- a globally canonical artifact identity

### 4. `artifact_id` is serving two roles today

It is currently both:

- the local artifact-store locator
- the main agent-facing artifact handle

That is convenient locally, but architecturally mixes:

- local storage concerns
- distributed identity concerns
- LLM ergonomics

### 5. `artifact_ref` is closer to the correct agent-facing abstraction

`artifact_ref` already provides:

- session/workflow/global scoping
- expiry / revocation
- digest-bound resolution
- separation between short handle and canonical record

This makes it a better primary handle for agents than raw `artifact_id`.

## Goals

1. Canonical identities must be content-only, not metadata-tainted.
2. Agent-facing handles must be short, scoped, and revocable.
3. Tenant and node semantics must be explicit, not overloaded onto existing IDs.
4. Local locators (`artifact_id`) are internal to the gateway store; agents never need them.

## Proposed Identity Layers

### Content

No structural change required.

| Role | Identifier |
|---|---|
| Canonical immutable identity | `sha256:<content-digest>` |
| Scoped convenience handle | none required beyond visibility rules |
| LLM-facing convenience handles | content name, short alias, `cnt_<alias>` |

### Artifacts

Split artifact identity into three explicit layers.

| Role | Identifier | Notes |
|---|---|---|
| Canonical immutable identity | `artifact_canonical_digest` | Must be identical across tenants/nodes for the same logical closure |
| Local storage locator | `artifact_id` | Short local store key; not canonical; may be node-local |
| Scoped agent-facing handle | `artifact_ref` | Preferred tool handoff handle |

### Gateway / Tenant / Provenance

| Role | Identifier | Notes |
|---|---|---|
| Build identity | `gateway.json` fields | Build provenance only |
| Node provenance | `node_id` | Federation/node origin only |
| Tenant / trust partition | `tenant_id` or existing `trust_domain`-like field | Required for real multi-tenant semantics |

## Canonical Artifact Identity

Introduce a new field:

- `artifact_canonical_digest`

It must be computed from canonical artifact closure only:

- artifact kind
- sorted file entries: `(name, content_handle)`
- sorted entrypoints
- sorted layers: `(layer_id, name, mount_path, digest)`

It must explicitly exclude:

- `artifact_id`
- `created_at`
- `builder_session_id`
- tenant-local paths
- node-local metadata

Semantics:

- same logical artifact closure anywhere -> same `artifact_canonical_digest`
- local rebuild of same closure -> same `artifact_canonical_digest`
- persisted manifest tampering -> still caught by current manifest integrity digest

Therefore we keep both:

- `artifact_digest`: stored-manifest integrity digest
- `artifact_canonical_digest`: distributed canonical artifact identity

## Artifact Locator and Scoped Handles

### `artifact_id`

`artifact_id` is a gateway-internal local store locator.

Properties:

- stable within one gateway store
- suitable for filesystem layout and CLI debugging
- not exposed to agents in any tool input or output
- may be referenced internally for store ops, SQLite joins, and operator CLI

### `artifact_ref`

`artifact_ref` is the only agent-facing artifact handle.

Properties:

- short (`ar.` + 12 hex chars)
- scoped: `session`, `workflow`, or `global`
- revocable
- expirable
- digest-bound (verified on resolution)

`artifact_ref` resolves to:

- `artifact_id` (internal; not returned to agents)
- `artifact_manifest_digest` (integrity)
- `artifact_canonical_digest` (distributed identity)
- provenance metadata

## API and UX Direction

### External model

Agents work exclusively in terms of:

- content: names / `cnt_...` / `sha256:` handles
- artifacts: `artifact_ref`

Gateway-internal code uses:

- `artifact_id` for store operations and local joins only

Auditing / federation / replication uses:

- `artifact_canonical_digest`

### Tool signatures

All artifact-accepting tools take `artifact_ref` as the sole artifact selector.

Target shape for all artifact tools:

```json
{ "artifact_ref": "ar.xxxxxxxxxxxx" }
```

Tools that currently expose `artifact_id` in their input or output schema are updated to:

- accept `artifact_ref` as input
- return `artifact_ref` and `artifact_canonical_digest` as output
- not expose `artifact_id` in agent-facing tool results

The only exception is the operator CLI, which may accept raw `artifact_id` for debugging.

## Where `artifact_id` Stays

`artifact_id` remains as a gateway-internal locator for:

- artifact store filesystem layout: `.gateway/artifacts/<artifact_id>/manifest.json`
- `ArtifactStore::inspect(artifact_id)` and `resolve_files(artifact_id)`
- SQLite joins: promotion records, agent revision records, artifact_refs rows
- operator CLI: `autonoetic artifact inspect <artifact_id>`

Everywhere else it currently appears in tool inputs/outputs, it is replaced by `artifact_ref` or `artifact_canonical_digest`.

## Multi-Tenant

Use:

- `node_id` for provenance and federation origin
- `artifact_ref` for scoped artifact handoff
- `sha256:` handles for canonical content identity
- `artifact_canonical_digest` for canonical artifact identity across nodes

Do not use:

- `gateway.json` as tenant identity (it is build provenance)
- `node_id` as authorization boundary (it is not secret)
- `artifact_id` as a globally canonical artifact identity (it is node-local)

For actual tenant isolation, a `tenant_id` field must be introduced explicitly and enforced at the API boundary, not inferred from existing IDs.

## Implementation Plan

Single implementation pass. No phases, no compatibility shims.

1. Add `artifact_canonical_digest` to `ArtifactBundle` and compute it from closure-only fields.
2. Drop `artifact_id` from all tool input schemas and tool output payloads.
3. Replace `artifact_id` in tool inputs with `artifact_ref`.
4. Return `artifact_ref` and `artifact_canonical_digest` from `artifact_build`, `artifact_inspect`, `resolve`.
5. Update `artifact_exec` and `artifact_prepare` to accept `artifact_ref` and bind approval reuse identity to `artifact_canonical_digest`.
6. `resolve` (the read door) reads a single artifact file by `ref="ar.<ref>"` plus a separate `file="<filename>"` argument — not a packed `art_<id>:<filename>` / `ar.<ref>:<filename>` string.
7. Update workflow implicit outputs to include `artifact_ref` instead of `artifact_id`.
8. Update all foundation prompt layers and agent playbooks to use `artifact_ref` exclusively.
9. Remove `artifact_id` from all agent-facing docs and tool descriptions.

## Code Areas To Modify

### Types

- `autonoetic-types/src/artifact.rs`
  - add `artifact_canonical_digest`
  - document `artifact_id` as local locator
  - extend `ArtifactRefRecord` to carry canonical digest

### Artifact store

- `autonoetic-gateway/src/artifact_store.rs`
  - add `artifact_canonical_digest` computed from closure-only identity (kind + sorted files + sorted entrypoints + sorted layers)
  - rename current `digest` field to `artifact_manifest_digest` to clarify it is integrity-only
  - return both digests on build / inspect

### Artifact tools

- `autonoetic-gateway/src/runtime/tools/artifact.rs`
  - `artifact_build`: return `artifact_ref` + `artifact_canonical_digest`; drop `artifact_id` from agent-visible output
  - `artifact_inspect`: accept `artifact_ref`; return canonical digest + manifest digest + files
  - `resolve`: one front door for any artifact/content handle; returns identity / files / content by `include` (replaces `artifact_resolve_ref`)

- `autonoetic-gateway/src/runtime/tools/artifact_exec.rs`
- `autonoetic-gateway/src/runtime/tools/artifact_prepare.rs`
  - accept `artifact_ref`; resolve to internal `artifact_id` inside the tool
  - bind approval reuse identity to `artifact_canonical_digest`

### Content tool integration

- `autonoetic-gateway/src/runtime/tools/content.rs`
  - artifact file addressing: `ref="ar.<ref>"` + separate `file="<name>"` (no packed `art_<id>:<file>` / `ar.<ref>:<file>`)
  - resolution path: resolve ref to `artifact_id`, then load file as before

### Workflow and handoff

- `autonoetic-gateway/src/scheduler/workflow_store.rs`
  - emit `artifact_ref` in child outputs; drop `artifact_id` from agent-visible output JSON

### Approval / execution identity

- `autonoetic-gateway/src/runtime/approved_exec_cache.rs`
  - shift artifact execution identity from `artifact:<artifact_id>` to canonical digest-based identity

### Promotion / revision linkage

- `autonoetic-gateway/src/runtime/promotion_store.rs`
- `autonoetic-gateway/src/runtime/tools/agent_revision.rs`
- `autonoetic-gateway/src/scheduler/gateway_store/agent_registry.rs`
  - keep `artifact_id` as internal join key in SQLite (no schema migration needed here)
  - use `artifact_canonical_digest` for any cross-node or federation-aware comparison
  - `agent_revision_create*` accepts `artifact_ref` externally, resolves to `artifact_id` internally

### Bootstrap / provenance

- `autonoetic-gateway/src/bootstrap.rs`
- config/docs for `node_id`
  - clarify `gateway.json` vs `node_id`
  - do not position either as tenant identity

### Docs

- `docs/content-store.md`
- `docs/AGENTS.md`
- `docs/ARCHITECTURE.md`
- `docs/agent-capabilities.md`
- relevant specs for workflow / credential / promotion flows

## Decision Summary

| Identity responsibility | Identifier | Scope |
|---|---|---|
| Content canonical identity | `sha256:<digest>` | universal |
| Artifact canonical identity | `artifact_canonical_digest` | universal |
| Artifact manifest integrity | `artifact_manifest_digest` (renamed from `digest`) | local |
| Artifact local store locator | `artifact_id` | gateway-internal only |
| Artifact agent handle | `artifact_ref` | scoped: session / workflow / global |
| Node provenance | `node_id` | federation metadata only |
| Build provenance | `gateway.json` fields | diagnostics only |
| Tenant partition | `tenant_id` (to be introduced) | ACL boundary |