# Layers and Artifacts

Two storage classes that are easy to conflate. An artifact is what an agent
**made**; a layer is what it **runs in**. They are addressed differently,
reviewed differently, and compose differently.

For the original design rationale see
[build-layers.md](../build-layers.md); this page describes the model as built.

## The one rule

**A layer is a content-addressed directory image bound to a mount path — not a
package set.**

It has no concept of packages, versions, or a resolution graph. Everything that
follows is a consequence of that one fact.

## Layer versus artifact

| | Artifact | Layer |
|---|---|---|
| Unit | a set of **named files**, each its own content handle | one **opaque directory image** |
| Addressing | per file (`ArtifactFileEntry.handle`) | whole-tree digest |
| Review | federation gates — auditor, static evaluator | network-provenance gate only |
| Meaning | content the agent authored | environment the agent needs |
| Mounted? | materialised as session content | bind-mounted read-only at `mount_path` |

`ArtifactLayer` (`autonoetic-types/src/layer.rs`) is the whole reference an
artifact holds:

```rust
pub struct ArtifactLayer {
    layer_id: String,
    name: String,
    mount_path: String,
    digest: String,
}
```

The store adds a zstd tarball plus a `LayerManifest` carrying
`resolved_packages`, `approval_scope`, file count and size.

## The model is deliberately agnostic

Because a layer is just a directory, these are all first-class with no special
handling:

- a `node_modules` tree from npm
- a Python virtualenv or a conda environment
- an unpacked language runtime (a Node tarball, a JDK)
- **compiled output** — a binary tree built inside the sandbox

Had layers been modelled as dependency trees, everything except the package
manager cases would be second-class. Treating the tree as opaque is what makes
compile-in-sandbox and conda work without a new mechanism.

## `resolved_packages` is provenance, not definition

`ResolvedPackage` entries are **scanned** out of the captured tree by walking
for filesystem markers — `*.dist-info` for Python, a `package.json` under
`node_modules/` for npm, `<name>-<version>/Cargo.toml` or
`.cargo-checksum.json` for Cargo, `<module>@<version>/` for Go.

Two consequences:

- It is descriptive metadata about what happens to be inside, never a manifest
  the layer is built from. A layer is valid with no packages at all.
- **Empty is legitimate** for a compiled or conda layer — nothing was resolved.
  That is different from a scanner that cannot see an ecosystem it should,
  which is a defect.

This set is what the approval boundary surfaces and what bless-on-promotion
freezes, so a scanner blind to an ecosystem silently freezes nothing while
reading as a complete closure.

## Layers mount together; they never merge

Several layers can be mounted in one sandbox. Nothing merges their contents —
each is its own read-only bind at its own `mount_path`, and composition happens
only through whatever search path the runtime already honours.

| Ecosystem | Runtime resolution | Composes across layers? |
|---|---|---|
| Python | `sys.path` via `PYTHONPATH` — flat list | yes, including cross-layer transitive imports |
| Node CJS | `NODE_PATH` | top-level only |
| Node ESM | directory walk; `NODE_PATH` is ignored | **no** |
| Cargo | none — resolved at build time | not applicable at exec |
| Go | none — resolved at build time | not applicable at exec |

Node ESM is the sharp edge. Its resolver never reads `NODE_PATH`, so a layer is
reachable only when its `node_modules` sits on the upward directory walk from
the running script. A deps layer mounted at the workspace root's `node_modules`
works; one mounted anywhere else fails with `ERR_MODULE_NOT_FOUND` even though
it is correctly mounted and on `NODE_PATH`.

Cargo and Go compile: their layers are build caches consumed by the toolchain,
which does its own resolution. There is nothing to compose at mount time.

## The rule that falls out

**A layer is the output of one resolution, never an input to another.**

Composing two independently-resolved layers would mean re-resolving —
reconciling versions, hoisting, handling peers — which is the package manager's
job and must not be reimplemented here. So:

> Installed together → one layer. Independent tools → separate layers.

`npm install a b` in a single packager step yields one tree that npm resolved,
where every module system works. Two separate steps yield two layers: both CLIs
run and both top-level packages import, but a package in one cannot see a
package in the other.

## Reuse and dedup

Layers are **not** scoped to the agent that built them. `built_by_agent_id` is
recorded in `LayerApprovalScope` for provenance and is read by no gate. The only
mount-time check compares the layer's build-time approved hosts against the
current session's grants — a supply-chain boundary, not an ownership one.

Reuse therefore works, but by **reference**, not discovery: an agent reaches a
layer only through its `runtime.lock` or the artifact it executes. There is no
mechanism to search the store.

Dedup is automatic and content-keyed: capturing an identical tree twice yields
the same `layer_id` and stores one archive, regardless of name or mount path.

## Not implemented: layers in capsules

As of 2026-09-09, capsule export **does not carry layers**. `CapsuleMode`
documents `Hermetic` as embedding artifact content *and layers*, but the export
path hardcodes an empty list, and the archive layout notes mark the `layers/`
directory as hermetic-only future work.

`included_artifacts` is empty too, and import never reads either field — so the
embedding that defines those modes is entirely unbuilt, not merely missing its
layer half.

Hermetic and Replay export therefore **refuse** rather than emit a capsule that
declares a closure it does not carry. Thin and Headless are defined as
reference-carrying and are what works today: the receiving gateway resolves the
artifacts and layers the `runtime.lock` names, so it must already hold them.

## Where the code is

| Concern | Location |
|---|---|
| Store, capture, dedup, package scan | `autonoetic-gateway/src/layer_store.rs` |
| Extraction and mounting into a sandbox | `autonoetic-gateway/src/runtime/tools/sandbox.rs` |
| Mount-time host-scope gate | `collect_layer_scope_issues` in the same file |
| Types | `autonoetic-types/src/layer.rs` |
| Pinning into an agent's closure | `autonoetic-types/src/runtime_lock.rs` |

Related: [content-visibility.md](content-visibility.md) for how session content
(not layers) becomes readable across sessions, and
[artifact-identity.md](artifact-identity.md) for how artifact digests are
computed.
