# Packager dependency determinism: resolve → validate → pin-on-promotion

- **Status:** Design (agreed direction; implementation scoped below)
- **Related:** RFC `docs/rfc/portable-wasm-execution-tier.md` §5.4 (P3), PR #447, constitution P-3.6 (read-only layers), promotion/approval machinery
- **Supersedes:** the RFC's standalone "bake step" (P3 increment 2/3) — see §1

## Context

While implementing P3 ("pinned dependency layers + lock/dev modes") we discovered
the proposed *bake step* (install deps → capture layer → rewrite `runtime.lock`)
was **redundant** with the existing packager flow, which already produces a
fully-locked closure end to end:

1. **packager** (`agents/specialists/packager.default/SKILL.md`) runs
   `sandbox.exec(..., capture_paths)` → installs deps (`pip install -r
   requirements.txt --target /tmp/venv`) → `LayerStore::create_from_dir` captures
   them as a content-addressed layer (`layer_id` + `digest`).
2. **`artifact.build`** embeds those layers into the artifact (`ArtifactBundle.layers`).
3. **`scaffold_runtime_lock_with_scopes`** (`runtime/install_contract.rs`) writes
   the artifact's layers into `runtime.lock.layers`; `agent_revision` **validates**
   `runtime.lock.layers == artifact.layers`.
4. At spawn, `script_execute` mounts those layers + sets `PYTHONPATH` — no pip.

So a properly-packaged script-agent already has `dependencies: []` and pinned
`layers: [...]`. The bake/lock code was therefore dropped from P3; what remains
in PR #447 is the genuinely-new, non-redundant piece: the **Hermetic-export
guard** (`RuntimeLock::is_dependency_locked()` + capsule export rejecting
runtime-pip deps), which *enforces* what the packager *produces*.

The real open problem — the subject of this note — is **determinism for
autonomy**: can the packager determine deps, build the layer, and lock the bundle
reliably enough to run unattended?

## Two determinism guarantees (keep them separate)

- **Run-time closure determinism — already solid.** The layer is the SHA-256 of
  its compressed contents; `runtime.lock` pins that digest; `agent_revision`
  validates lock == artifact; the runtime mounts exactly those bytes. A built
  closure runs identically forever, offline, tamper-evident — *independent of how
  non-deterministic the agent that built it was.*
- **Build determinism — not guaranteed.** Two sources of non-determinism:
  1. **Which deps** — from `requirements.txt` (not free LLM inference), but the
     agent/author still authors it.
  2. **Which versions** — `pip install -r requirements.txt --target` resolves
     *latest at install time* unless pinned. The layer digest freezes whatever
     resolved, but a *re-build* later yields different versions/digest, and the
     layer stores **no human-readable resolved-version manifest** (digest only).

## The model: resolve → validate → pin-on-promotion

Pinning is a **consequence of a validated, approved run — never a precondition.**
Do **not** block on an unpinned `requirements.txt`.

1. **Build (unpinned OK, non-blocking).** Packager installs whatever resolves →
   the layer digest pins the content. **Also capture the resolved set** (e.g.
   `pip freeze` of the layer's `site-packages` / dist-info) as **read-only
   provenance** stored with the layer. Cheap, non-blocking, and it makes the run
   auditable.
2. **Run + audit.** The agent executes against that layer; execution traces /
   causal chain record it (existing machinery).
3. **Validate + promote + approve.** The existing promotion gate runs; the
   approver sees the **concrete resolved versions** being blessed
   (`requests==2.31.0`, …), not a vague "latest".
4. **Pin-on-promotion ("bless the found versions").** On approval, record the
   resolved set as the agent's blessed dependency lock. The digest was always the
   runtime pin; this adds the reproducible, human-readable version manifest —
   *earned by validation.*

This fits the separation of powers: the agent stays semantic/free during
iteration; the **freeze is mechanical, at the promotion boundary**, gated by
audit + approval.

### Caveat designed for

Capture provenance **at build (always)**, not only at promotion — otherwise an
unblessed-but-working agent whose layer is later pruned by the retention policy
becomes unrecoverable. Build-time capture is the safety net (always
reconstructable); promotion is the trust/freeze. "Unpinned" must never mean
"unrecoverable", only "not yet blessed".

### Verification gate

"Validated" must be concrete: a post-bake check that the layer actually satisfies
the agent — imports resolve / smoke test passes (reuse `unit_test_runner` /
evaluator/auditor) — **before** promotion. An autonomous build must not be able
to ship a broken-but-locked closure.

## Implementation plan (increments)

1. **Build-time resolved-version provenance.** After the packager captures a
   dependency layer, record the resolved set (`pip freeze` equivalent) into the
   layer metadata/manifest (`autonoetic-types/src/layer.rs`, `LayerStore`).
   Read-only; no behavior change to install or run.
2. **Surface resolved versions at the approval boundary.** Include the layer's
   resolved-version manifest in the promotion/approval payload so the operator
   blesses a concrete set.
3. **Bless-on-promotion.** On approval, persist the blessed pinned set on the
   agent revision (see open question below).
4. **Verification gate.** Wire a post-bake import/smoke check into the
   promotion flow; block promotion (not build) on failure.

Each increment is independently shippable and verified on bubblewrap + docker
(availability-gated, RFC §4.1).

## Open questions (proposed defaults)

- **Where the blessed pin lives.** *Default:* resolved-version **provenance
  manifest inside the layer** (always, at build) + a **blessed lock recorded on
  the revision** at promotion. (Alternative considered: a re-derived
  `requirements.lock` in the artifact bundle.)
- **Re-bake from the blessed pin** (cross-arch / portability). *Default:* out of
  scope now; revisit in P4, where WASI layers need a reproducible rebuild from the
  pinned set anyway.

## Non-goals

- Replacing the packager or the `capture_paths` → `artifact.build` flow (it stays
  the mechanism).
- Forcing upfront version pins / hash-pinning as a precondition (rejected — see
  the model).
