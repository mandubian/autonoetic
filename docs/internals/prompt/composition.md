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
Foundation → Guidance blocks (standing) → Tool bridging → Persona → User profile
   → Agent instructions (SKILL.md body) → Output contract
   → Guidance blocks (earned this session)
```

(then runtime tails are appended: prior-knowledge memory, degradation notice,
state attestation.) Assembly lives in
`context::compose_system_instructions_full`.

Guidance renders in **two** places, and the split is a cache property, not
presentation. Blocks whose activation is fixed at spawn go in the standing
section. Blocks that can activate *mid-session* — phase-gated ones, §2.1 — go in
a tail section after the output contract, at the very end of the cache prefix.
Anything that appears mid-session invalidates every cached byte after it, and the
standing section is followed by the agent's entire `SKILL.md`; from the tail, each
new block is a pure append. See `guidance::ComposedGuidance`.

The guiding principle of the factorization:

> **`SKILL.md` carries role *intent*** — what this agent is for, its decision
> logic, its verdict rubrics. **Everything mechanical** — how to call a tool,
> how to resume, how to format output — is contributed by the tool/capability/
> role it belongs to and rendered by the gateway.
>
> Litmus test: *if two agents would write the same sentence, it does not belong
> in either `SKILL.md`.*
>
> Second litmus test (`docs/internals/prompt/burden-study.md`): *if most
> sessions never reach the situation this sentence describes, it should not be
> in the prompt from turn 1.* Gate it — §2.1.

There are **three** prose mechanisms, each for a different kind of content.

**Every addition costs tokens on every turn.** The fixed prompt is measured by
`tests/prompt_composition_budget.rs`, which prints a per-layer breakdown and
enforces a steady-state ceiling per agent:

```bash
cargo test -p autonoetic-gateway --test prompt_composition_budget -- --nocapture
```

If that test fails, a change added weight paid on every turn. The fix is
normally to gate the addition, not to raise the ceiling.

**Before adding doctrine, apply these three tests in order**
(evidence and worked examples in [`prompt-burden-study.md`](burden-study.md)):

1. **Ownership.** Does the agent holding this tool have the full capability set
   for the flow it belongs to? If it has to bounce to another agent mid-flow —
   and no privilege is being contained by that bounce — the tool is in the wrong
   place. *Moving it beats compressing it:* that single question returned 9% of
   the planner's prompt, against 1.3% for a whole compression pass.
2. **Phase.** If most sessions never reach the situation this prose describes, it
   should not be in the prompt from turn 1. Gate it — §2.1 for call mechanics,
   §2.2 for role doctrine.
3. **Litmus.** If two agents would write the same sentence, it belongs in
   neither `SKILL.md`.

And three anti-patterns, each of which cost real time to learn:

- **Moving prose into a `ToolPresent` block does not save tokens.** The block
  fires exactly when the tool is advertised — i.e. exactly when the description
  would have been in the prompt. Move it for organisation; gate it on a *phase*
  for size.
- **Do not restate a schema field in its own tool description,** and do not
  pre-load a rule the gateway already enforces with a repair hint.
- **Never name internal types in doctrine that asks an agent to branch on a
  payload.** State serialized values (`"do_not_retry"`, not `DoNotRetry`) and say
  whether the field can be absent. Naming the Rust variant is a silent no-op at
  runtime.

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
    Phase(&str),            // the session has reached a milestone — see §2.1
    All(Vec<…>), Any(Vec<…>), Not(Box<…>),
}
```

`compose_guidance(blocks, ctx)` filters by `when`, **dedupes by `id`** (first
wins), and returns a `ComposedGuidance { standing, phase_tail }`. `standing` is
ordered by `priority` then `id`; `phase_tail` is ordered by **fact arrival**
(§2.1). The `GuidanceContext` is the live-turn facts:

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
- `phase` — how far the session has progressed (§2.1). `None` on composition
  paths with no live session (bootstrap, static analysis), so `Phase` never
  matches there.

### 2.1 Phase gating — the only condition that changes mid-session

Every other condition is decided at spawn, which makes a block gated only on them
effectively static: prose that *might* matter at turn 40 is paid for at turn 1.
`GuidanceCondition::Phase` gates on `SessionPhase` — a monotonic set of facts the
gateway derives from what it observes:

| Fact | Proven by |
|---|---|
| `artifact_built` | `artifact_build` succeeded; **or** a succeeded child's state / an artifact-domain tool result carries an `artifact_ref` or `reuse_guards.has_coder_artifact` |
| `gate_verdict_recorded` | `promotion_record` succeeded |
| `revision_seeded` | `agent_revision_create[_from_intent]` succeeded |
| `child_spawned` | `agent_spawn` succeeded |
| `credential_configured` | `credential_setup` succeeded |

Rules that keep this sound — respect them when adding a fact:

- **Mechanical only** (P-5.14). Facts come from gateway-observed state, never
  from agent prose. A result must parse, must not be a failure, and the agent's
  own action must carry an explicit `"ok": true`.
- **Monotonic.** Facts are never retracted, so a block cannot flicker in and out
  and churn the prompt cache. A condition that could toggle back does not belong
  in the tail.
- **Two derivation sites.** `SessionPhase::observe` reads tool results;
  `SessionPhase::observe_gateway_signal` reads child-state notifications and
  workflow joins, which arrive as turn-start messages and never become tool
  results. The second is the primary path for a yield-based planner.
- **Evidence is allowlisted** (`tool_emits_artifact_evidence`) to artifact-domain
  tools, so an unrelated tool that echoes an `artifact_ref` cannot widen a fact
  that many blocks depend on.
- **Phase never widens reach.** Blocks are still collected only from tools that
  passed the tier/capability filter, so `Phase` can only further restrict.
  Guarded by `agents_without_the_tool_never_see_its_procedure`.

Give phase-gated blocks `priority: PHASE_GATED_PRIORITY_FLOOR`. Placement in the
tail is what protects the cache; the floor keeps priorities legible and makes
mis-gating visible in review.

Full rationale and the migration plan: `docs/internals/prompt/burden-study.md`.

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
| `federation.escalate_procedure` | `federation_escalate` | ToolPresent **+ Phase** `artifact_built` | how to escalate: read verdicts via `promotion_query`, seed the revision first, worked payload — renders in the phase tail |

### 2.2 Section gates — phase gating for `SKILL.md` role doctrine

Guidance blocks carry *mechanical* doctrine (how to call a tool). Role doctrine
lives in the `SKILL.md` body, and gets the same phase axis through a frontmatter
`sections:` declaration:

```yaml
sections:
  - heading: "Evaluation Federation"
    when: phase(artifact_built)
```

Each entry names a top-level `##` heading and the phase that must be reached
before it enters the prompt. A gate carries its `###` subsections with it.

**This evicts; `<!-- extended -->` only defers.** The extended half is inlined
permanently from the first tool call, so that split saves exactly one turn. A
gated section is *absent* until its phase lands — a planner that never builds
anything never pays for the federation doctrine at all.

Earned sections render in the **phase tail** beside phase-gated guidance, in
fact-arrival order, not back in their original position (re-inserting in place
would shift every cached byte after them).

**Gates are declared in frontmatter, not as inline markers, because they are
validated.** `SkillParser::parse` rejects four ways of being wrong, each of
which would otherwise fail silently at runtime:

| Rejected | Why it matters |
|---|---|
| heading not present in the body | a renamed heading would silently stop gating |
| unknown phase fact (checked against `ALL_PHASE_FACTS`) | a typo'd gate would never fire |
| unparseable `when` | ditto |
| duplicate gate for one heading | composition uses `.find`, so the later one would be inert |

Composition is deliberately *forgiving* where parsing is strict: an unmatched
gate is ignored at compose time rather than failing closed, because failing
closed would strip prose from a live session. The error belongs where it can
name the file.

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
`docs/reference/response-contract.md` for the schema shape). A manifest
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

- `docs/archived/agent-prompt-factorization.md` — the roadmap/history and the
  rationale for what was migrated, what was intentionally left role-specific
  (e.g. unittest policy, no-network, single-pass discovery), and why foundation
  unification (item E) was deferred.
- `docs/reports/2026-07-19-comparison-hermes-agent.md` — the prior-art study the edit-format and
  doctrine-centralization ideas drew from.
