# Design: dev/test dependency grade

**Status:** proposed (design only — no implementation in this PR)
**Author:** (drafted with Claude)
**Date:** 2026-06-11
**Related:** `docs/AGENTS.md` (script-agent testing), the packager / layer flow
(`autonoetic-gateway/src/layer_store.rs`, `runtime/tools/artifact.rs`),
`autonoetic-gateway/src/runtime/tools/artifact_exec.rs` (test execution path),
`autonoetic-gateway/src/capsule/export.rs` (capsule closure).

## Problem

An agent's tests often need libraries the agent does **not** use at runtime —
the canonical case is `pytest`. Today the dependency model is **single-grade**:

- `RuntimeLock` carries `dependencies: Vec<LockedDependencySet { runtime, packages }>`
  and `layers: Vec<LockedLayerMount>`. Both *are* "the runtime closure."
- The promotion `unit_test_runner` runs in a **no-network** sandbox via
  `artifact_exec`, which mounts the artifact's `layers` and sets `PYTHONPATH`.
  With no network, a test dependency **cannot** be installed at test time — so to
  be importable it must already live in those layers.
- Those same layers are the shipped capsule (and `CapsuleMode::Hermetic`/`Replay`
  *require* a locked layer closure, embedding the layers verbatim).

Net: **any dependency needed to run the tests ends up in the runtime capsule.** A
test-only framework bloats every capsule (pytest + plugins ≈ tens of MB) and
widens the supply-chain/attack surface for zero runtime benefit, and there is no
mechanism to strip it.

Interim mitigation (shipped separately): steer test authors to stdlib `unittest`,
which needs no dependency. That removes the *common* case but not the *general*
one — some agents legitimately want richer test tooling (pytest fixtures,
`hypothesis`, coverage) that they don't ship.

## Goals

- Let an agent declare **test-only** dependencies that are available to the
  promotion test run but **excluded from the runtime closure and the capsule**.
- Keep the **runtime** closure (and its determinism / pin-on-promotion behavior)
  exactly as it is today.
- Test deps are still **audited** (resolved, recorded, content-addressed) — they
  are excluded from the *shipped* closure, not from scrutiny.
- Additive and back-compatible at the data level (serde defaults), so existing
  agents/locks/capsules are unaffected.

## Non-goals

- A general multi-environment matrix (prod/staging/…); just two grades:
  **runtime** and **test**.
- Network at test time. Test deps are baked into a layer at build time, same as
  runtime deps — the no-network test sandbox is unchanged.
- Solving native-dep portability (orthogonal; see the WASM-tier RFC).

## Proposed model

Introduce a **layer role** discriminator and route it through the four execution
surfaces. A "test" layer is built and audited like any other, but mounted only
for test runs and never embedded in the runtime closure or capsule.

### 1. Types (additive, default = `Runtime`)

```rust
// autonoetic-types/src/layer.rs
pub enum LayerRole { Runtime, Test }   // serde default: Runtime

// add `#[serde(default)] role: LayerRole` to:
//   LayerManifest, CapturedLayer, ArtifactLayer
```

```rust
// autonoetic-types/src/runtime_lock.rs
// Keep `layers` as the RUNTIME closure (semantics unchanged). Add a sibling:
pub struct RuntimeLock {
    // ...
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub test_layers: Vec<LockedLayerMount>,
}
// (Alternative: a single `layers` list with a `role` field on LockedLayerMount.
//  A separate `test_layers` keeps every existing reader of `layers` — capsule
//  export, runtime spawn — correct with no change. Preferred for that reason.)
```

`LockedDependencySet` similarly gains an optional `kind: LayerRole` (default
`Runtime`) so the human-readable lock reflects which packages are test-only.

### 2. Authoring surface

The agent author declares test deps in one of the conventional places the
packager already inspects for dependency files (`requirements.txt`,
`pyproject.toml`, `package.json`, …):

- **Python:** `requirements-dev.txt`, or `[project.optional-dependencies]`
  `test`/`dev` group, or PEP 735 `[dependency-groups]`.
- **JS:** `package.json` `devDependencies`.

The packager maps these to `LayerRole::Test`; everything else is `Runtime`. No
new manifest schema is strictly required — it can key off ecosystem-standard
dev-dependency conventions, which is the least surprising for authors.

### 3. The four execution surfaces

| Surface | Mounts | Why |
|---|---|---|
| **Packager bake** (`layer_store::create_from_dir`) | builds runtime layer **and** a separate test layer (role=Test) at a distinct mount path | test deps captured + content-addressed, kept separate |
| **Promotion test run** (`artifact_exec`, used by `unit_test_runner`) | runtime layers **+ test layers**; `PYTHONPATH` spans both | the only place test deps are needed |
| **Runtime spawn** (production agent execution) | **runtime layers only** | test deps never enter a running agent |
| **Capsule export** (`capsule/export.rs`, Hermetic/Replay closure) | **runtime layers only**; test layers excluded from the embedded closure | test deps never ship |

`artifact_exec` already gathers `bundle.layers` and builds `PYTHONPATH`
(`runtime/tools/artifact_exec.rs`); the change is to include test layers **only**
on the test-execution path. Production spawn and the capsule closure traversal
filter to `role == Runtime`.

### 4. Promotion / determinism

- The existing pin-on-promotion flow (`blessed_packages`, `scan_resolved_packages`,
  `aggregate_resolved_packages`) blesses **runtime** resolved packages into the
  lock as today. Test-layer `resolved_packages` are recorded for audit and may be
  blessed into `test_layers`, but they do **not** enter the runtime closure.
- Determinism is unchanged for the runtime closure. Test layers are
  content-addressed too, so a changed test dep doesn't churn the *runtime* digest
  (they're separate layers).

### 5. Security / audit

Test deps are **not** a blind spot: they're resolved, version-recorded, and
content-addressed exactly like runtime deps, and remain subject to the
supply-chain sentinel. They are merely **excluded from the artifact that ships**.
This is strictly better than today, where the only way to run pytest-based gate
tests is to ship pytest.

## Migration / back-compat

Pre-release, so we can change shapes freely, but the design is additive anyway:

- New enum/field default to `Runtime` / empty via serde, so existing manifests,
  locks, artifacts, and capsules deserialize unchanged and behave identically.
- An agent with no test deps produces no test layer — identical output to today.

## Alternatives considered

1. **Stdlib-only, no grade (status quo + the unittest steer).** Cheapest; fully
   solves the common case. Doesn't serve agents that legitimately want pytest/
   hypothesis/coverage they don't ship. This design is the escalation for those.
2. **Single `layers` list with a `role` tag** (no separate `test_layers`).
   Slightly less duplication, but every existing reader of `layers` (capsule
   export, runtime spawn) must learn to filter or it silently ships test deps —
   more places to get wrong. Rejected in favor of the separate-list default.
3. **Naming-convention layers** (e.g. a layer named `test-*` is excluded).
   Implicit and fragile; a typo ships test deps. Rejected.

## Phased implementation (when greenlit)

1. **Types + serde defaults** — `LayerRole`, `test_layers`, `kind`; no behavior
   change yet. Round-trip tests.
2. **Packager** — detect dev/test dependency files → build a role=Test layer.
3. **Execution split** — `artifact_exec` test path mounts test layers; production
   spawn and the capsule closure filter to runtime-only. Tests on both paths.
4. **Lock + promotion** — `test_layers` in the lock; bless runtime deps as today,
   record test-layer provenance separately.
5. **Docs** — update `docs/AGENTS.md` (replace the "stdlib only" interim note with
   "declare test deps in the dev/test group; they won't ship").

## Open questions

- **Authoring source of truth:** auto-detect ecosystem dev-dependency groups
  (least friction) vs. an explicit manifest `test_dependencies` block (most
  explicit). Leaning auto-detect, with an explicit override if needed.
- **Mount path** for the test layer (a fixed `/opt/autonoetic-test-deps`?) and
  `PYTHONPATH` ordering relative to runtime layers.
- **Does the lock need `test_layers` at all,** or is recording them on the
  artifact (not the runtime lock) sufficient? The lock is the runtime closure;
  test layers may belong only on the artifact/promotion record.
- **JS/Javy:** dev dependencies are largely moot for the per-agent Javy model
  (each agent is a standalone module); this grade is primarily a Python/native
  concern. Confirm scope.
