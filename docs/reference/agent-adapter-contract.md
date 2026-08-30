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
- `--output-dir <path>` (optional): writes generated files to disk.

### Generated files

Always:

- `SKILL.md`
- `runtime.lock` (sha256 computed on first gateway load)

Conditionally (when mapping is needed):

- `scripts/pre_map.py`
- `scripts/post_map.py`

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
  - reads completion request JSON from stdin,
  - parses the last user message content as JSON,
  - applies all inferred input mappings (`from -> to`) when possible.
- `post_map.py`:
  - reads completion response JSON from stdin,
  - parses `response.text` as JSON,
  - applies all inferred output mappings in reverse (`to -> from`) so caller
    receives target shape.

If parsing fails, scripts are fail-soft and pass data through unchanged.

Script-mode wrappers (#1222): script-mode agents can now carry the same
`middleware` block, with the hooks running at the script boundary —
`pre_process` receives the normalized task payload on stdin and its stdout
replaces it before the entry script runs; `post_process` receives the entry
script's stdout and its stdout becomes the reply. The script-mode contract is
verbatim stdin→stdout (no `CompletionRequest`/`CompletionResponse` envelope);
a hook failure there is fail-closed (the turn fails) rather than fail-soft.
Note the generator's current scripts read the LLM envelopes, so on a
script-mode agent they pass data through unchanged — emitting
script-mode-aware hooks is a follow-up for the generator.

## Capability Inheritance

When `--base-manifest-json` includes `capabilities`, the generated wrapper
places those capabilities into wrapper frontmatter so wrapper policy remains
compatible with the base specialist security envelope.

## Runtime Notes

- Middleware runs relative to the wrapper agent directory.
- Wrapper generation is deterministic for the currently implemented schema diff
  and required-field mapping strategy.
- For complex schema transforms (nested objects, arrays, one-to-many mappings),
  manual refinement of generated scripts is expected.

## Validation

Current tests covering adapter script behavior:

- `autonoetic-gateway/tests/agent_adapter_scripts_integration.rs`
- `autonoetic-gateway/tests/agent_adapter_wrapper_integration.rs`
