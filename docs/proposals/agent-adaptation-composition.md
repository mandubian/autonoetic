# Agent Adaptation vs Composition — Wrapper Provenance, Drift, and Discovery

**Status:** Partial — 2026-08-27 (Phase 0 shipped; Phases 1–3 open)
**Builds on:** [`../reference/agent-adapter-contract.md`](../reference/agent-adapter-contract.md),
middleware hooks (`autonoetic-gateway/src/runtime/middleware.rs`),
federation carry-forward (`autonoetic-gateway/src/runtime/federation_carry_forward.rs`),
[`implicit-artifacts-agent-evolution.md`](implicit-artifacts-agent-evolution.md)
**Found in:** audit of the middleware/adapter surface, 2026-08-27

---

## 1. Terminology: adaptation ≠ composition

The codebase currently conflates two concepts that must be specified separately,
because one is implemented and the other is silently dropped.

| | **Adaptation** | **Composition** |
|---|---|---|
| What it is | Runtime **behavior**: deterministic transformation hooks around an agent's turn | Static **relationship**: a derivation edge, wrapper → base |
| Manifest field | `middleware: { pre_process, post_process }` | `adapter: { base_agent_id, … }` |
| Granularity | Per turn — runs around **every** LLM completion | Once — written at generation time, immutable with the revision |
| Executes? | Yes — sandboxed scripts (`runtime/middleware.rs:50-207`) | **No** — never interpreted at runtime; consumed by gates, roster, and audit tooling |
| Answers | "How does this agent transform its I/O?" | "Where did this agent come from, against which base revision, and why?" |
| Parser status | Fully parsed (`runtime/parser.rs:33-52`) | **Silently dropped** — no `AgentManifest` field exists (`autonoetic-types/src/agent.rs:278-279`) |
| Failure mode | None known — feature works | Lost lineage, undetectable staleness, uncheckable capability derivation |

The dependency runs one way only:

- An agent may declare `middleware` **without** being composed — a self-normalizing
  hook wraps nothing (`examples/archived/specialized_builder/sample_agent/SKILL.md:14-15`).
- An agent may be composed **without** middleware — e.g. a delegation wrapper whose
  body is "map the task, `agent_spawn` the base, map the reply" (see §5).
- Neither field implies the other. `middleware` is *how it runs*; `adapter` is
  *what it derives from*.

## 2. What exists today

**Adaptation is done.** `Middleware { pre_process, post_process }` is typed
(`autonoetic-types/src/agent.rs:508-517`), parsed from both frontmatter shapes
(`runtime/parser.rs:33-52, 278`), executed sandboxed around each LLM completion
(`runtime/lifecycle.rs:3508-3521` pre, `3991-4004` post), hardened against
label-stripping (`lifecycle.rs:3522-3560`), wired through all four spawn paths
(`execution.rs:4247, 4949, 5114, 5271`), and carried by install/revision tooling.
`skip_llm` short-circuits a round with correct budget semantics
(`lifecycle.rs:3562-3605`).

**The adapter bundle works end-to-end.** `agent-adapter.default` runs
`schema_diff.py` → `generate_wrapper.py` → `artifact_build` → delegates install to
`specialized_builder.default` (one door, P-9.15). E2E-tested
(`tests/agent/adapter_wrapper.rs`).

**The generated wrapper is a re-embodiment fork, not a delegating wrapper.**
`generate_wrapper.py` copies the base's **capability list verbatim** into the
wrapper frontmatter (`generate_wrapper.py:144-148, 213-214`) and embeds only the
first 2000 characters of the base SKILL body as its instructions
(`generate_wrapper.py:193-195`). The wrapper never spawns the base; it *is* a
self-contained copy of the base's envelope, with middleware mapping caller I/O
onto it. The base agent need not even be installed at wrapper runtime.

## 3. Gaps

1. **Composition metadata is dropped.** `generate_wrapper.py:170-173` emits an
   `adapter:` block (`base_agent_id`, `generated_at`, `schema_notes`); the parser
   silently discards it (`AgentManifest` has no `deny_unknown_fields`).
   [`../reference/agent-adapter-contract.md`](../reference/agent-adapter-contract.md)
   §Wrapper Traceability claims lineage, debugging, and audit are "enabled" by this
   block — none of that is true of any *parsed* manifest today; the data survives
   only as raw SKILL.md bytes.

2. **Forks go stale invisibly.** The wrapper freezes the base's capabilities and a
   2000-char instruction excerpt at generation time. When the base is promoted to a
   new revision — new instructions, new capabilities, new `io` contract — nothing
   detects, reports, or routes regeneration. A caller keeps using a wrapper that
   maps onto a schema the base no longer has.

3. **Capability derivation is unchecked.** Wholesale inheritance means the wrapper's
   envelope equals the base's — governed only by the generic P-2.25
   capability-delta approval at install. Nothing relates the wrapper's capability
   set to its derivation: a fork *gaining* power relative to its base is not even
   surfaced on the promotion card.

4. **The planner cannot know adaptation exists.** `planner.default`'s foundational
   table (`agents/lead/planner.default/SKILL.md:152-167`) and decision flow contain
   no `agent-adapter` row or step; the `adapt` decision rule lives only in
   `skill-crystallizer.default` (operator-triggered via `/crystallize`) and in
   operator-facing docs (`docs/guide/human-agent-collaboration.md:331`). The
   archived design intended the planner to carry the rule
   (`docs/archived/plan_adapt.md:172-188`); the current SKILL.md lost it.

5. **`self_describe` does not expose the route.** Adaptation appears only nested
   inside the operator-triggered `skill_crystallization` path
   (`runtime/tools/self_describe.rs:101-106, 131-140`). An agent asking "how can I
   adapt an existing agent?" gets no direct answer.

6. **Stale docs.** `docs/internals/install-pipeline.md:555-563` calls hooks
   "operator-configured" (they are agent-manifest-declared) with outdated line
   citations; `docs/proposals/data-envelopes-egress-localization.md:161` cites
   outdated lifecycle.rs lines.

## 4. Proposal

### Phase 0 — First-class provenance (bug fix)

Promote the dropped block to a typed field:

```rust
pub struct AdapterProvenance {
    pub base_agent_id: String,
    /// Promoted revision digest of the base at generation time.
    /// `None` = unknown at generation — under-claim, never guess.
    pub base_revision_digest: Option<String>,
    pub generated_at: String,
    pub schema_notes: Vec<String>,
    pub generator: String, // e.g. "agent-adapter.default"
}
```

- Add `adapter: Option<AdapterProvenance>` to `AgentManifest`
  (`autonoetic-types/src/agent.rs:278-279`); parse both frontmatter shapes in
  `runtime/parser.rs` (mirror `test_parse_middleware_hooks`, parser.rs:686).
- Round-trip through `render_skill_document` (`runtime/install_contract.rs:235-274`)
  and `agent_revision_create_from_intent` (accepted arg → manifest → install-intent
  digest, `runtime/tools/agent_revision.rs:619, 2292, 2775`).
- Classify as a **contract** field in `federation_carry_forward.rs:66-78`, matching
  `middleware`: changing provenance voids carry-forward.
- `generate_wrapper.py` records the base's currently promoted revision digest at
  generation time (sourced via `agent_inspect` by the delegating caller, or by the
  adapter itself); absent information stays `None`.
- Tests: parser round-trip; adapter e2e asserts provenance survives
  artifact → revision → inspect.

### Phase 1 — Discovery

- **`planner.default` SKILL.md**: add an `agent-adapter.default` row to the
  Foundational Agents table and a Decision Flow step mirroring the crystallizer's
  rule verbatim (`agents/evolution/skill-crystallizer.default/SKILL.md:140` —
  "an existing agent's behaviour fits, but callers reshape its I/O every time; the
  tactic is a mapping, not a judgment"). Keep the one door: adapter → artifact →
  `agent-factory.default` install; adaptation never bypasses federation.
- **`self_describe`**: add an `EvolutionPath` `agent_adaptation` to
  `EVOLUTION_PATHS` (`runtime/tools/self_describe.rs:112-147`) with
  `PathEnactor::Pipeline(["agent-adapter.default", "agent-factory.default"])` —
  agent-reachable (any `AgentSpawn` holder can start it), availability derived
  from installed agents by the existing under-claiming rules
  (`pipeline_unavailable_reason`, self_describe.rs:234-257). Not
  `OperatorPipeline`: the only operator gate on the route is the standard P-2.25
  promote approval every install already has.
- Fix the stale doc citations from §3.6; add the missing `middleware` row (and the
  new `adapter` row) to the key-field table in `docs/AGENTS.md`.

### Phase 2 — Composition-aware checks (all advisory, nothing new executes)

- **Drift signal**: `agent_inspect` / `agent_list` gain `adapter` provenance plus a
  computed `stale_base: true` when `base_revision_digest` differs from the base's
  currently promoted revision (or the base is gone). The planner decision-flow rule
  for a stale wrapper: route regeneration to `agent-adapter.default`; do not keep
  delegating through it.
- **Derivation delta on the promotion card**: for manifests carrying `adapter`
  provenance, `agent_revision_promote`'s P-2.25 card gains a "derived from"
  section — base id, digest match/mismatch, and the capability delta *vs the base*.
  A fork gaining power becomes a visible line item. Advisory, not a hard reject —
  a wrapper may legitimately narrow.
- **Lineage enumeration**: the roster surfaces `adapter.base_agent_id`, so "find
  all wrappers derived from X" is one filtered `agent_list`.

### Phase 3 — Re-adapt loop (open)

When a base with known wrappers is promoted, the gateway could file a signal to
`evolution-steward.default` (or an `anomaly_flag`) proposing re-adaptation. Left
open: automation shape, and whether staleness should ever escalate from advisory
to blocking.

## 5. What is explicitly NOT proposed

- **No runtime composition engine.** No pipeline/graph executor, no declarative
  multi-agent choreography. Runtime composition is `agent_spawn` orchestration,
  with control flow decided by the LLM and scheduling owned by the gateway; a
  declared graph executor would be a second control plane crossing separation of
  powers.
- **No capability amplification through composition.** Composition metadata never
  grants power; every derived agent still passes the one door (P-9.15 install,
  P-2.25 delta approval). Phase 2's derivation check is observability, not a new
  privilege source.
- **No new execution semantics.** Middleware remains the only adaptation
  mechanism; `adapter` metadata is never interpreted at runtime.
- **Delegation-style wrappers stay legal.** A wrapper holding only `AgentSpawn`
  and delegating to the base alias (instead of re-embodying it) is a valid
  composition style the same provenance field covers — always current by alias
  resolution, but pays child-spawn orchestration cost (Ri-0.14 wake cycles). The
  proposal standardizes the *metadata*, not the style.
- **Script-mode middleware and true `skip_llm` bypass stay out of scope** —
  sandbox-spawn-semantics surgery, separate ticket.

## 6. Validation

- Parser unit test: `adapter` provenance round-trips both frontmatter shapes.
- `self_describe` guard test: `agent_adaptation` path availability derives from
  installed agents (present / missing / unverifiable).
- Adapter e2e (`tests/agent/adapter_wrapper.rs`): provenance survives install.
- Staleness: `agent_inspect` reports `stale_base` after base re-promotion.
- Planner SKILL.md wording matches the crystallizer's `adapt` row verbatim.
