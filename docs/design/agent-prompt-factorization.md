# Agent-prompt factorization: where to cut, compress, and de-entangle

Status: proposal / roadmap. Builds on the guidance-block mechanism
(#463–#466). Pre-release project — breaking changes are acceptable; this doc
favors the correct end-state over back-compat.

## The core problem

A composed system prompt today is assembled from **four parallel mechanisms**:

1. `foundation_*.md` — capability-gated `include_str!` layers (`context.rs`).
2. **Guidance blocks** — tool/capability/role/model-gated (`guidance.rs`, #463–#465).
3. **`SKILL.md` prose** — the agent body, inlined verbatim.
4. **`io.returns` / output_policy** — output contract rendered from manifest.

Four mechanisms means doctrine lands wherever the author happened to put it, and
the same rule gets restated in several of them. The `SKILL.md` bodies have grown
large and **entangled** — generic mechanism (how to call a tool, how to resume,
how to format output) is woven into role intent (what this agent is *for*).

Sizes (body words / lines):

| Agent | words | lines |
|---|---|---|
| planner.default | 5030 | 545 |
| agent-factory.default | 3014 | 355 |
| coder.default | 2864 | 402 |
| specialized_builder.default | 2819 | 459 |
| planner.collaborative | 1798 | 335 |
| sealed_evaluator.default | 1460 | 240 |

Recurring `##` sections across the fleet (copy-paste signal): `Output Format`
×15, `Behavior` ×12, `Resumption` ×6, `Clarification` ×6, `Content System` ×3,
`Running Code` ×3, `Recording Promotion` ×3, `Injection defense` ×2.

The #466 series proved the pattern: when doctrine was centralized, we found and
fixed **three latent inaccuracies** (forbidden-commands overstatement,
`promotion_record` required-fields, approval field names) that had drifted in
the per-agent copies. Duplication isn't just bloat — it rots.

## Target end-state

`SKILL.md` should contain **only role intent**: what the agent is for, its
decision logic, its unique reasoning and verdict rubrics. Everything mechanical
should be contributed by the tool/capability/role it belongs to and rendered by
the composer. The litmus test: *if two agents would write the same sentence, it
does not belong in either `SKILL.md`.*

---

## Recommendations (quick wins → hard changes)

### A. Finish migrating the recurring mechanical sections (low risk)

Same pattern as #466, one block per cluster, owned by the relevant tool/capability:

- **Clarification protocol** (×6) → a block gated `Always` (or `ToolPresent("user_ask")`):
  "when you lack a fact the operator must supply, return `clarification_needed`
  / `user_ask` and end the turn; don't spin discovery tools." Several agents
  restate this near-verbatim.
- **Content System / resolve usage** (×3) → fold the remaining `resolve`-by-name
  and visibility doctrine into `content_write` / `resolve` guidance (the
  `name`+`content` rule already moved in #471).
- **Injection defense** (×2), **artifact-first handoff** → capability-gated blocks.

Effort: small each. Net: removes the highest-count duplications.

### B. Base + role-variant blocks — unlocks the "role-divergent" clusters (medium)

The clusters #466 left behind (unittest, no-network, single-pass) are
*role-divergent*, not duplicated — that's why a single block can't hold them.
Add a **base + variant** convention using the existing conditions:

```
test-framework (base, Role∈{coder,unit_test_runner}): "Python gate tests use stdlib unittest."
  ├─ Role(coder):            "...never add pytest/nose to requirements (capsule bloat)."
  └─ Role(unit_test_runner): "...prefer the stdlib runner; use pytest only if vendored."
```

`compose_guidance` already orders by priority and dedups by id, so a base block
(low priority) followed by role variants (higher priority) renders cleanly. This
lets us migrate no-network and single-pass too without flattening nuance.

### C. Separate output structure (schema) from output pragmatics (block) (medium)

`Output Format` is the single most-repeated section (×15) and overlaps the
`io.returns` mechanism that *already* renders an "Output Contract." But this is
**not** a clean delete-the-prose migration like the others, because the prose
and the schema are not equivalent:

- **`io.returns` expresses structure** — fields, types, `required`, enums. It is
  machine-checked and drives output validation and the repair loop. It is
  load-bearing; downstream consumers depend on it.
- **The prose expresses pragmatics** — *which* field to use, *when*, tone,
  worked examples, and (for orchestrators) the fact that different situations
  emit different shapes (e.g. planner's operator-facing chat reply vs a spawn
  handoff). A JSON Schema cannot hold judgment like "put walkthroughs in
  `summary`, never nested in `result`."

The fix is **not** "schema replaces prose" (that loses the pragmatics) and **not**
"remove `io.returns`" (that loses enforcement). It is a clean **separation of
ownership**, deleting only the overlap:

| Concern | Owner | Notes |
|---|---|---|
| Fields, types, `required`, enums, JSON skeleton | **`io.returns`** (rendered + enforced) | single source of truth for structure |
| *Which* field / *when* / tone / shape-by-situation | **guidance block / role intent** | the judgment a schema can't encode |
| One-field hint | schema `description` (terse) | lives with its field; **not** cross-field rules |

Rule of thumb: *if it could be a JSON Schema keyword, it belongs in `io.returns`;
if it is "do X, not Y," it is guidance.* Delete the structural half of every
`## Output Format` section (it just restates the schema and is the drift source).

**The lever that collapses most of the pragmatics:** the recurring judgment is
almost always the same envelope — **operator-facing prose in a `summary` field;
flat string facts in a `result` field; never nest walkthrough trees.**
Standardize that `summary`+`result` envelope as **one shared convention** (a
single block, or a documented envelope) instead of ~15 per-agent restatements.
Then most agents need **no `## Output Format` section at all** (schema for
structure + the shared convention), and only the few orchestrators keep a short
role-specific note about their chat-vs-handoff shapes.

Symptom to fix while doing this: `planner.default` is `returns_enforcement:
advisory` and currently crams a cross-field rule into a field `description`
("…No nested walkthrough trees — use `summary`…"). Pull that sentence *out* into
the shared convention block; leave the `description` terse.

### D. Shared orchestrator doctrine block (medium)

`planner.default`, `agent-factory`, `evolution-orchestrator`,
`improvement-orchestrator` repeat orchestration mechanics: tool-vs-agent
invocation, "spawn → end turn → resume on wake" (Ri-0.14), don't-loop-`workflow_wait`,
reuse-before-spawn. Extract an **orchestrator block** gated on
`Capability("agent_spawn")`. Each orchestrator keeps only its own decision flow
(planner's federation/promotion gate; factory's pipeline stages). Likely the
single biggest line reduction, concentrated in the largest files.

### E. HARD: unify foundation layers into the block registry (larger, breaking)

`foundation_*.md` and guidance blocks are two mechanisms doing the same job
(capability-gated prompt prose). Collapse foundation into the block registry as
`Always`/`Capability`-gated builtin blocks. One ordering model, one dedup model,
one place to look. Deletes `compose_foundation` and the `include_str!` set;
`context.rs` composes a single ordered block stream (foundation blocks at the
lowest priority band). Touches the system-prompt assembly, so do it behind the
existing prompt snapshot tests.

### F. HARD: regression guard so doctrine can't drift back (small, high-leverage)

The root cause of the original mess is that nothing *stops* an author from
pasting doctrine into a `SKILL.md`. Add a test that scans every `agents/**/SKILL.md`
and fails if it contains migrated-doctrine fingerprints (e.g. "Forbidden shell
commands", "requires both `name` and `content`", "approval_ref set to",
"never restart from scratch"). Each centralized block registers its fingerprints;
the test asserts no `SKILL.md` re-introduces them. This is what makes the
factorization *stick* instead of re-rotting.

### G. Measurement (small)

Add a `trace prompt-size <agent>` (or a test that dumps composed-prompt token
counts per agent) so we can see before/after and catch regressions in prompt
budget. The planner's body alone is ~6–7k tokens every turn; D+C+E should cut a
large fraction fleet-wide.

---

## Suggested sequencing

1. **F (regression guard)** first — cheap, and it backstops everything after.
2. **A** (finish recurring sections) — proven pattern, immediate dedup.
3. **B** (base+variant) — closes the role-divergent clusters #466 deferred.
4. **C** (separate output structure/pragmatics; standardize the `summary`+`result`
   envelope) — removes most of the ×15 `Output Format` sections.
5. **D** (orchestrator block) — biggest reduction in the largest files.
6. **E** (unify foundation+blocks) — the structural simplification; do last,
   behind prompt snapshot tests.

## Risks / guardrails

- **Don't over-centralize.** Role-specific reasoning, verdict rubrics, and
  decision flow MUST stay in `SKILL.md` — that's the agent's value. The litmus
  test (two agents would write the same sentence) is the boundary.
- **Verify against enforcement when migrating** — every #466 migration found a
  prose/reality mismatch. Check `policy.rs`, tool schemas, and response shapes,
  not the old prose.
- **Snapshot the composed prompt** for a few representative agents (planner,
  coder, auditor) before E so structural changes are diff-reviewable.
- **One mechanism, gated** beats "delivered everywhere" prose: blocks only
  appear when the tool/capability/role is actually present, so factoring also
  *narrows* prompts (agents stop carrying doctrine for tools they don't have).
