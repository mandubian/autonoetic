# Agent Adapter Specialist (`agent-adapter.default`)

This document explains how the adapter specialist works, with emphasis on the
Python scripts it uses to compare schemas and generate wrapper agents.

## Purpose

`agent-adapter.default` is an evolution-layer specialist that creates a
**wrapper agent** around an existing specialist when:

- the base specialist is close to the requested role, but
- I/O schema or behavior shape does not match the target contract.

The wrapper keeps the base behavior reusable while introducing deterministic
input/output remapping middleware where needed.

## High-Level Flow

1. Receive:
   - `base_agent_id`
   - `target_spec` (target accepts/returns schemas and adaptation intent)
   - optional `rationale`
2. Read base `SKILL.md` and extract base manifest/schema metadata.
3. Run `schema_diff.py` to detect compatibility and mapping needs.
4. Run `generate_wrapper.py` to produce wrapper artifacts. The wrapper inherits
   the base agent's inference settings (its `llm_config`, else its
   `llm_preset`, else the `agentic` preset) with `temperature: 0.0` — a wrapper
   is a transformation layer, so it must reason with the base's model and never
   pin one of its own.
5. Build the wrapper bundle with `artifact_build`, then **delegate installation
   to `specialized_builder.default`** — the adapter holds no `AgentRevision`
   capability, so `agent_revision_create` / `agent_revision_promote` calls from
   it are rejected by the policy engine. `specialized_builder.default` is the
   only agent licensed to call those tools (the one-door invariant, P-9.15).
6. Return wrapper id and mapping summary.

## Files in Adapter Bundle

- `agents/evolution/agent-adapter.default/SKILL.md`
- `agents/evolution/agent-adapter.default/runtime.lock`
- `agents/evolution/agent-adapter.default/scripts/schema_diff.py`
- `agents/evolution/agent-adapter.default/scripts/generate_wrapper.py`

## Script: `schema_diff.py`

Compares base and target I/O schemas and emits mapping requirements.

### Input (stdin JSON)

```json
{
  "base_accepts": {},
  "base_returns": {},
  "target_accepts": {},
  "target_returns": {}
}
```

All fields may be `null`.

### Output (stdout JSON)

```json
{
  "accepts_compatible": false,
  "returns_compatible": false,
  "requires_input_mapping": true,
  "requires_output_mapping": true,
  "input_mappings": [
    {"from": "task", "to": "query"},
    {"from": "topic", "to": "domain"}
  ],
  "output_mappings": [
    {"from": "result", "to": "summary"},
    {"from": "score", "to": "confidence"}
  ],
  "notes": ["..."]
}
```

### Mapping heuristic

The script infers deterministic mappings when both sides are object schemas with
required fields:

- same-name fields are paired first (`x -> x`),
- remaining fields are paired by required-field order
  (`target_required[i] -> base_required[i]`).

In that case:

- input mappings are inferred as `target_required -> base_required`,
- output mappings are inferred similarly and reversed by generator in post-map.

If schemas are missing or types mismatch, mappings can be empty and notes explain
why manual refinement is required.

## Script: `generate_wrapper.py`

Generates wrapper agent files from base skill text + schema diff metadata.

### CLI arguments

- `--base-skill <path>`: path to base skill markdown.
- `--base-agent-id <id>`: base agent identifier for traceability.
- `--wrapper-id <id>`: generated wrapper agent id.
- `--target-spec-json <json>`: wrapper target I/O schema object.
- `--schema-diff-json <json>`: output from `schema_diff.py`.
- `--base-manifest-json <json>` (optional): used to inherit base capabilities.
- `--base-schema-json <json>` (optional): the base's declared
  `{"accepts": …, "returns": …}` schemas. Without it, round-trip validation is
  skipped and the verdict under-claims to `partial`.
- `--base-revision-digest <digest>` (optional): promoted revision digest of the
  base at generation time.
- `--wrapper-mode reasoning|script` (optional, default `reasoning`): script
  mode emits a deterministic wrapper around a copy of the base's script entry
  (see [Script-mode wrappers](#script-mode-wrappers)).
- `--base-script-path <path>` (script mode): the base agent's installed
  `script_entry` file, copied verbatim into the wrapper bundle.
- `--fail-soft` (optional): emit legacy fail-soft mapping hooks instead of the
  default fail-loud ones. Discouraged.
- `--output-dir <path>` (optional): writes generated files to disk.

### Generated files

Always:

- `SKILL.md`
- `runtime.lock` (sha256 computed on first gateway load)

Conditionally (when a mapper was generated for that direction):

- `scripts/pre_map.py`
- `scripts/post_map.py`

Script mode additionally generates:

- `scripts/entry.py` (the executable entry shim)
- `scripts/base_entry.py` (the digest-pinned copy of the base's entry)

### Mechanical verdict (round-trip validation)

Nothing LLM-judged decides whether a generated mapping works. For every
generated mapper the generator:

1. builds a **synthetic payload** from the declaring schema (required fields
   only, type-respecting, deterministic — strings take their field name),
2. executes the *emitted* hook file against it (same bytes that ship),
3. validates the mapped output against the other side's schema with a
   stdlib-only validator (required fields, primitive types, enums, one
   object/array level).

The stdout JSON carries the mechanically derived result:

```json
{
  "wrapper_id": "base.agent.adapter",
  "wrapper_mode": "reasoning",
  "requires_input_mapping": true,
  "requires_output_mapping": true,
  "verdict": "ok",
  "validation_failures": [],
  "notes": ["accepts: round-trip validation passed (1 rename(s) proven …)"],
  "files": ["SKILL.md", "runtime.lock", "scripts/pre_map.py", "scripts/post_map.py"]
}
```

Verdicts, and what the adapter must report (its `io.returns.status` mirrors
them — the adapter's SKILL.md pins this mapping):

| Verdict | Meaning | Mapper emitted? |
|---------|---------|-----------------|
| `ok` | every emitted mapper proven on a synthetic payload | yes |
| `partial` | mapper emitted but unproven — validation failed (paths in `validation_failures`), or skipped because a schema was unavailable | yes |
| `clarification_needed` | no trustworthy mapper exists — schema missing on one side, nothing derivable, or every inferred rename type-invalid | no (for that direction) |

Additional mechanical guards:

- **Type-guard on renames**: a mapping pair whose two sides declare differing
  property types (e.g. `string` vs `integer`) is dropped, not emitted — a
  confidently wrong rename is worse than no rename. Dropped pairs are named in
  notes.
- **Refuse, don't passthrough**: when mapping is required but no mapper can be
  generated, the generator no longer emits an identity passthrough hook under
  `status: "ok"` — the passthrough fork must not masquerade as an adapter.
- **Optional-field gaps**: optional target fields not covered by any rename
  pass through unmapped; the note names them.

### Fail-loud mapping hooks

Generated mappers default to fail-loud: a payload that violates the declared
contract (unparseable JSON, non-object payload, missing a required mapped
field) makes the hook exit non-zero, so the turn **fails closed** — in script
mode the hook failure fails the turn; in reasoning mode the middleware error
fails the completion. The old behavior — silently passing untransformed data
through so the LLM improvises — is available only via `--fail-soft`, and is
discouraged. Absent *optional* fields never fail.

### Script-mode wrappers

`--wrapper-mode script` produces a wrapper that is deterministic end-to-end
(#1251): the composition cheap path — mapping at the payload boundary without
ever paying for a completion.

Design: **bundle copy** (shape 1). The base's installed `script_entry` file is
copied verbatim into the wrapper bundle (`scripts/base_entry.py`); the
wrapper's `script_entry` is a generated shim (`scripts/entry.py`) that runs
the copy in-process via `runpy`, so stdin/stdout/environment/argv pass through
verbatim and the gateway's spawn accounting stays at the #1222 shape (pre
hook, entry, post hook — up to three spawns, no new fee surface). The hooks
are written to the script-mode contract: verbatim stdin→stdout, no
`CompletionRequest`/`CompletionResponse` envelope.

Why a copy rather than a sandbox mount or exec-by-alias: the generated bundle
must be self-contained (exec-by-alias is impossible — script mode has no
tools), and the copy is pinned by `adapter.base_revision_digest`, so the
existing drift machinery — roster `stale_base`, promotion-time
`adapter_drift_detected` events, `adapter_drift_notice` — works unchanged when
the base is re-promoted. A sandbox mount would need operator-approved
`allowed_mount_roots` and wouldn't travel with the bundle.

Capability containment: the wrapper inherits the base's *declared* capabilities
through the existing `--base-manifest-json` copy, so whatever the entry shim
executes is governed by the wrapper's own manifest — the base's privileges are
not smuggled through composition. The P-2.25 promotion card's `derived_from`
capability delta is empty when the adapter copies capabilities, and shows the
delta when it doesn't. Isolation overrides are derived from the wrapper's
capabilities, never the base's.

Mechanical refusals (exit code 2, `verdict: "clarification_needed"`, no files
written): missing/absent `--base-script-path`, non-`.py` base entry,
base source that does not compile, or a target spec without an object
`io.accepts` (script agents without an input contract are rejected at
install — the generator refuses earlier with the same verdict). The manifest
carries `execution_mode: "script"`, `script_entry`, the base's
`script_input_mode` when declared, the middleware block, and the same
`adapter:` provenance (including the base revision digest) as reasoning-mode
wrappers.

Single-file boundary: the copy ships the entry script only. A base whose entry
imports sibling modules from its bundle needs manual refinement — the import
fails loudly at runtime rather than misbehaving silently.

### Wrapper Traceability

Generated `SKILL.md` includes an `adapter` metadata section (parsed as
`AgentManifest.adapter` — `AdapterProvenance` in `autonoetic-types/src/agent.rs`;
proposal `docs/proposals/agent-adaptation-composition.md`):

```yaml
adapter:
  base_agent_id: "researcher.default"
  base_revision_digest: "rev_sha256:..."   # optional — base revision at generation time
  generated_at: "2024-03-12T10:30:00Z"
  schema_notes: ["accepts: compatible", "returns: target requires additional fields"]
  generator: "agent-adapter.default"
```

This enables:
- Lineage tracking: find all wrappers derived from a base agent
- Debugging: understand why a wrapper was created
- Audit: timestamp and schema diff summary for governance

Consumers (#1202): `agent_list` and `agent_inspect` surface the provenance plus a
computed `stale_base` verdict (the base was re-promoted or removed since
generation — regenerate the wrapper); `agent_revision_promote`'s P-2.25 card
gains a `derived_from` section with the capability delta *vs the base*. All
advisory — nothing blocks a spawn or promotion on these signals.

Re-adapt loop (#1221): a base promotion that stales installed wrappers emits one
`revision.adapter_drift_detected` causal event listing them, and `agent_spawn`
on a stale wrapper attaches a `gateway_note` proposing regeneration (or a
deliberate pin). Still advisory — staleness never blocks.

Proactive operator notice (#1228): the same promotion also pushes an
`adapter_drift_notice` entry into the promoting root's operator-activity feed
(severity `attention`, linked to the drift causal event — exactly-once per
promotion via the feed's unique causal index, subject to the standard per-root
rate limit) and returns an `adapter_drift` block in the promote response, so
drift reaches the operator and the promoting session without anyone querying
the store. Advisory like every other drift surface: the notice proposes
re-adaptation, it never implies auto-regeneration — enactment stays
adapter → artifact → factory → one door (P-9.15/P-2.25). A steward-spawn
escalation ladder (feed first, steward if unactioned) remains a possible
follow-up, deliberately not built speculatively.

The block is **contract-classified** for federation carry-forward (like
`middleware`): changing provenance voids a carry, because it names what the
gates verified against. An absent `base_revision_digest` means unknown at
generation time — provenance under-claims, never guesses.

### LLMConfig Design

Wrappers hardcode `temperature: 0.0` regardless of base agent settings. This is
intentional: wrappers are **transformation layers**, not reasoning agents. The
base agent provides reasoning; the wrapper only maps I/O schemas via middleware
scripts. Deterministic settings ensure consistent transformation behavior.

### Runtime.lock

The generated `runtime.lock` has an empty `sha256` field. The gateway computes
the actual hash on first load and caches it. This allows wrapper generation
without requiring gateway binaries at generation time.

### Middleware behavior in generated scripts

- `pre_map.py`:
  - reasoning wrappers: reads the completion request envelope from stdin,
    parses the last user message content as JSON, applies the inferred input
    mappings (`from -> to`) in place;
  - script wrappers: reads the verbatim task payload from stdin and writes the
    mapped payload to stdout.
- `post_map.py`:
  - reasoning wrappers: reads the completion response envelope from stdin,
    parses `response.text` as JSON, applies the inferred output mappings in
    reverse (`to -> from`) so the caller receives the target shape;
  - script wrappers: reads the entry script's verbatim stdout payload and
    writes the target-shape payload to stdout.

Generated mappers are fail-loud by default (see above); `--fail-soft` restores
the legacy pass-through-on-mismatch behavior.

Script-mode wrappers (#1222/#1251): script-mode agents run the same
`middleware` block at their payload boundary — `pre_process` receives the
normalized task payload on stdin and its stdout replaces it before the entry
script runs; `post_process` receives the entry script's stdout and its stdout
becomes the reply. The contract is verbatim stdin→stdout, and the generator's
script-mode hooks are written to exactly that contract. A failing hook is
fail-closed (the turn fails), hooks inherit the entry script's isolation
overrides and emergency-stop registration, and the run's egress label covers
the hook scripts too.

## Capability Inheritance

When `--base-manifest-json` includes `capabilities`, the generated wrapper
places those capabilities into wrapper frontmatter so wrapper policy remains
compatible with the base specialist security envelope.

## Runtime Notes

- Middleware runs relative to the wrapper agent directory.
- Wrapper generation is deterministic for the currently implemented schema diff
  and required-field mapping strategy — and every generated mapping is proven
  (or named as unproven) by the mechanical round-trip verdict.
- For complex schema transforms (nested objects, arrays, one-to-many mappings)
  the generator either proves the flat rename mechanically or refuses/downgrades
  the verdict; manual refinement of generated scripts is expected there.

## Validation

Current tests covering adapter script behavior (domain binary
`autonoetic-gateway/tests/agent/`, #922):

- `adapter_scripts.rs` — generator/diff CLI contracts, inference inheritance,
  digest-capture instruction pinning, verdict-instruction pinning
- `adapter_roundtrip.rs` — round-trip validation verdicts, fail-loud hook
  behavior, refusal paths
- `adapter_script_wrapper.rs` — script-mode wrapper generation and hooks
- `adapter_wrapper.rs` — generated wrapper execution through the LLM path,
  provenance parsing
- `adapter_real_adaptation.rs` — install → adapt → promote → drift loop with
  the real scripts
- `adapter_staleness.rs` — roster `stale_base` verdicts

Script-mode wrapper end-to-end (spawn through the gateway with no LLM):
`autonoetic-gateway/tests/script/middleware_hooks.rs`.
