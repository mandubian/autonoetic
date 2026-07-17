# Agent prompt composition & guidance

How an agent's system prompt is assembled, where each piece of doctrine lives,
and how to add or change it without re-introducing the "built on the fly"
duplication this system was designed to remove.

Code: `autonoetic-gateway/src/runtime/guidance.rs` (mechanism),
`autonoetic-gateway/src/runtime/context.rs` (composition + foundation),
`autonoetic-gateway/src/runtime/foundation_*.md` (foundation prose),
`autonoetic-gateway/tests/skill_doctrine_guard.rs` (regression guard).

## The composed system prompt

Each turn, the gateway builds one system message by layering, in order:

```
Foundation → Guidance blocks → Tool bridging → Persona → User profile
   → Agent instructions (SKILL.md body) → Output contract
```

(then runtime tails are appended: prior-knowledge memory, degradation notice,
state attestation.) Assembly lives in
`context::compose_system_instructions_full`.

The guiding principle of the factorization:

> **`SKILL.md` carries role *intent*** — what this agent is for, its decision
> logic, its verdict rubrics. **Everything mechanical** — how to call a tool,
> how to resume, how to format output — is contributed by the tool/capability/
> role it belongs to and rendered by the gateway.
>
> Litmus test: *if two agents would write the same sentence, it does not belong
> in either `SKILL.md`.*

There are **three** prose mechanisms, each for a different kind of content.

## 1. Foundation layers (static, manifest-gated)

`foundation_*.md` files, embedded at compile time and selected by
`context::compose_foundation` based on the **manifest alone** (capabilities +
execution mode). They are *already* centralized (one file each, included into
every matching agent) — not duplicated prose — so they are kept as-is.

| Layer | File | Included when |
|---|---|---|
| Core | `foundation_core.md` | always |
| Workflow | `foundation_workflow.md` | has `AgentSpawn`, or **not** script mode |
| Artifact | `foundation_artifact.md` | has `WriteAccess` |
| Script | `foundation_script.md` | execution mode is `Script` |
| Digest | `foundation_digest.md` | has `WriteAccess` with a `digest`/`*` scope |
| SDK | `foundation_sdk.md` | `CodeExecution`, **`AgentSpawn`**, or role **`architect`** / **`static_evaluator`** |

Gating is locked by `test_compose_foundation_*` tests in `lifecycle.rs`.

Foundation is the right home for doctrine that depends only on the manifest and
is largely static. It is rendered inside `compose_system_instructions_full`, so
**every** prompt-composition path includes it (including non-lifecycle callers).

## 2. Guidance blocks (dynamic, turn-gated)

For doctrine that depends on the **live turn** — which tools are actually
advertised, which model is running, the agent's role — use a guidance block.
This is the mechanism in `runtime/guidance.rs`.

```rust
pub struct GuidanceBlock { id, prose, when: GuidanceCondition, priority }

pub enum GuidanceCondition {
    Always,
    Capability(&str),       // capability *kind*, e.g. "write_access" (see capability_kind)
    ToolPresent(&str),      // tool name is in the advertised set this turn
    ModelFamily(&[&str]),   // case-insensitive substring vs the model id
    Role(&str),             // agent role (id segment before the first '.')
    All(Vec<…>), Any(Vec<…>), Not(Box<…>),
}
```

`compose_guidance(blocks, ctx)` filters by `when`, orders by `priority` (then
`id` for determinism), **dedupes by `id`** (first wins), and renders the prose
joined by blank lines. The `GuidanceContext` is the live-turn facts:

- `capabilities` — from the manifest.
- `active_tool_names` — the **final advertised** tool set (after MCP merge,
  `tool_discover`, dedupe, and the max-tools cap), so `ToolPresent` matches what
  the model actually sees.
- `model_family` — the resolved model id (`lifecycle::resolved_model_id`, which
  is **preset-aware**: agents declare an `llm_preset`, not a pinned model, so it
  reads the resolved inference profile, not `manifest.llm_config`). `ModelFamily`
  substring-matches it (`["claude"]` matches `claude-opus-4-8`).
- `role` — the agent id segment before the first `.` (e.g. `coder.default` →
  `coder`).

### Where blocks come from

- **Tool-contributed** — a `NativeTool` implements `fn guidance() -> Vec<GuidanceBlock>`.
  Collected (`NativeToolRegistry::collect_guidance_blocks`) **only from tools that
  survived the tier/capability filter**, so a block can't appear unless its tool
  could. This keeps guidance and capability in lockstep.
- **Builtin** — `guidance::builtin_blocks()` holds cross-cutting doctrine not
  owned by any single tool.

The lifecycle gathers both, builds the `GuidanceContext`, and renders the
"Guidance" section **after** the tool list is built (so `ToolPresent` sees the
real advertised set).

### Current blocks

| Block id | Owner | Gate | Doctrine |
|---|---|---|---|
| `clarification.ask_or_default` | builtin | `Always` | don't fabricate a missing fact — ask / `clarification_needed` and end turn; else sensible default |
| `content.write_protocol` | `content_write` | `ToolPresent` | `content_write` requires both `name` and `content` |
| `editing.content_patch` | `content_patch` | WriteAccess + ToolPresent | prefer `content_patch` over re-writing whole files |
| `editing.content_patch.format.replace` | `content_patch` | `Not(ModelFamily[gpt,codex])` | prefer `mode="replace"` (default for all non-gpt/codex models) |
| `editing.content_patch.format.v4a` | `content_patch` | `ModelFamily[gpt,codex]` | prefer `mode="v4a"` for multi-entry edits |
| `sandbox.forbidden_commands` | `sandbox_exec` | `ToolPresent` | gateway-blocked commands (matches `policy.rs`) |
| `exec.approval_continuation` | `sandbox_exec` + `artifact_exec` | exec tool present **and not** a promotion-gate role | `approval_required` → return `request_id`, retry with `approval_ref` on resume |
| `promotion.record_protocol` | `promotion_record` | `ToolPresent` | how to call `promotion_record` (role+pass required) |
| `resumption.workflow_state_first` | `workflow_state` | `ToolPresent` | on wake, call `workflow_state` first; never restart |
| `orchestration.coordinate_children` | `agent_spawn` | `ToolPresent` | yield/Ri-0.14: spawn async → end turn → auto-wake; one `workflow_wait` join; never poll |

## 3. Output contract (driven by `io.returns`)

Output *structure* is owned by the agent's `io.returns` schema, not by prose.
When an agent declares `io.returns`, `compose_system_instructions_full` renders
an "Your Output Contract" section: *"your ENTIRE final reply must be a single
raw JSON object matching this schema. No prose before or after the JSON, and no
markdown code fences."* Agents therefore **must not** restate that — `io.returns`
delivers it once.

Enforcement (`returns_enforcement`) governs **`io.returns` schema** validation:
`strict` (a schema violation blocks the response; default for script agents) or
`advisory` (schema violations are logged and emitted as causal events but do not
block; default for reasoning agents). Non-schema `output_policy` constraints
(e.g. `prohibited_text_patterns`, size limits) are a separate mechanism and are
enforced regardless of this mode.

**Imported AgentSkills** rarely declare `io.returns`. On import
(`runtime/parser.rs`), when a skill has no `io.returns`, it gets a permissive
default envelope (`{status, summary, result}`), and `returns_enforcement`
**defaults to `advisory`** (an explicit choice, if any, is preserved). So the
schema never hard-fails output from a skill we don't control, while the skill
still hands off a predictable shape and inherits the Output Contract instruction.

**Gateway-injected `anomalies` field.** The same parser choke point that
synthesizes the imported-skill envelope also augments any **reasoning**
agent's *declared* object-shaped `io.returns` with a required `anomalies`
array — "anything unexpected?" as a schema field rather than a virtue (see
`docs/response-validation-gate.md` for the schema shape). A manifest
declaring its own `anomalies` property wins untouched; script agents are
excluded. The rendered Output Contract gets one extra line naming it a
standing witness contract. This doctrine sentence is fingerprinted in
`skill_doctrine_guard.rs` — do not restate it in a SKILL.md body either.

## The regression guard

`tests/skill_doctrine_guard.rs` scans every `agents/**/SKILL.md` and fails if it
contains a fingerprint of doctrine that has been centralized. This is what keeps
the factorization from re-rotting — without it nothing stops an author from
re-pasting a rule into a manifest. Each centralized rule registers a distinctive
fingerprint (e.g. `"Forbidden shell commands"`, `"wrap JSON in markdown code
fences"`, `"never restart from scratch"`).

## How to add or change doctrine

1. **Pick the owner by what the doctrine depends on:**
   - manifest only, static → a **foundation** layer.
   - a specific tool / model / role / live turn → a **guidance block**
     (`NativeTool::guidance()` if a tool owns it; `builtin_blocks()` if it's
     cross-cutting).
   - output shape → the agent's **`io.returns`** schema.
2. **Gate it precisely** with `GuidanceCondition` (prefer `ToolPresent` /
   `Capability` over `Always`; use `Not`/`All`/`Any` to exclude or combine —
   e.g. promotion-gate roles are excluded from `exec.approval_continuation`).
   For base + variant (e.g. per model family), use **distinct ids** and priority
   ordering — `compose_guidance` dedupes by id, so a shared id would drop variants.
3. **Trim the duplicated prose** from every `SKILL.md` that restated it; leave
   only genuinely role-specific elaboration.
4. **Verify against real enforcement** — the migration repeatedly found prose
   that overstated the actual rule (`policy.rs` blocklist, tool schema `required`,
   response field names). Check the source, not the old prose.
5. **Register a fingerprint** in `skill_doctrine_guard.rs` so it can't drift back.

## See also

- `docs/design/agent-prompt-factorization.md` — the roadmap/history and the
  rationale for what was migrated, what was intentionally left role-specific
  (e.g. unittest policy, no-network, single-pass discovery), and why foundation
  unification (item E) was deferred.
- `docs/comparison-hermes-agent.md` — the prior-art study the edit-format and
  doctrine-centralization ideas drew from.
