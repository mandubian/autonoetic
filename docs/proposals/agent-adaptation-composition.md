# Agent Adaptation vs Composition — Wrapper Provenance, Drift, and Discovery

**Status:** Partial — 2026-08-28 (Phases 0–3 shipped; Phase 3 steward/operator notification follow-up open)
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
| Parser status | Fully parsed (`runtime/parser.rs:33-52`) | First-class since Phase 0: `AgentManifest.adapter` → `AdapterProvenance` (#1204) |
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

1. ~~Composition metadata is dropped~~ — **resolved by Phase 0 (#1204).**
   The `adapter:` block now parses as `AgentManifest.adapter`
   (`AdapterProvenance`) and survives install (see §4.1). Before Phase 0,
   `generate_wrapper.py:170-173` emitted an `adapter:` block that the parser
   silently discarded — the lineage/audit claims in
   [`../reference/agent-adapter-contract.md`](../reference/agent-adapter-contract.md)
   §Wrapper Traceability were not true of any parsed manifest; the data
   survived only as raw SKILL.md bytes.

2. ~~Forks go stale invisibly~~ — **resolved by Phase 2 (#1202).** The roster
   (`agent_list` both paths, `agent_inspect`) computes `stale_base` against the
   base's currently promoted revision, and the P-2.25 promotion card gained a
   `derived_from` section. The planner decision flow carries the regenerate-on-stale
   rule.

3. ~~Capability derivation is unchecked~~ — **resolved by Phase 2 (#1202),
   advisory.** `derived_from_payload` surfaces `added_vs_base` /
   `removed_vs_base` on the promotion card. A hard reject remains out of scope
   by design (§5: composition metadata never grants or blocks power).

4. ~~The planner cannot know adaptation exists~~ — **resolved by Phase 1 (#1203).**
   `planner.default` now carries an `agent-adapter.default` row in its Foundational
   Agents table, a Decision Flow step (10a), and a Discovery-section pointer. Before
   Phase 1 the `adapt` decision rule lived only in `skill-crystallizer.default`
   (operator-triggered via `/crystallize`) and operator-facing docs
   (`docs/guide/human-agent-collaboration.md:331`); the archived design had intended
   the planner to carry the rule (`docs/archived/plan_adapt.md:172-188`).

5. ~~`self_describe` does not expose the route~~ — **resolved by Phase 1 (#1203).**
   `EVOLUTION_PATHS` now advertises `agent_adaptation` as an agent-reachable
   pipeline (`agent-adapter.default` → `agent-factory.default`), with availability
   derived from installed agents like every other path. Before Phase 1, adaptation
   appeared only nested inside the operator-triggered `skill_crystallization` path.

6. ~~Stale docs~~ — **resolved by Phase 1 (#1203).** `docs/internals/install-pipeline.md`
   A.4 corrected (hooks are agent-manifest-declared, not operator-configured; line
   refs fixed), `data-envelopes-egress-localization.md` pre-hook citation corrected,
   `docs/AGENTS.md` key-field table gained `middleware` and `adapter` rows.

## 4. Proposal

### Phase 0 — First-class provenance (bug fix)

**[x] shipped (#1204).** Promote the dropped block to a typed field (shipped
shape: `generated_at`/`generator`/`base_revision_digest` are `Option` — absent
information parses and under-claims):

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

**[x] shipped (#1203).**

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

**[x] shipped (#1202).**

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

### Phase 3 — Re-adapt loop

**[x] core shipped (#1221).** When a base is promoted, the gateway now closes
the loop deterministically — the LLM never has to happen upon staleness:

- **One drift event per promotion** (`revision.adapter_drift_detected` causal
  event): `find_stale_wrappers_for_base` scans installed wrappers' provenance
  after `atomic_promote` and lists every wrapper the move staled. One event
  per promotion, never per wrapper; under-claiming wrappers are not listed
  (unknown is not stale).
- **Spawn-time advisory**: `agent_spawn` on a stale wrapper attaches a
  `gateway_note` (truncation-exempt) naming the claimed vs current digest and
  the two lawful moves — regenerate via `agent-adapter.default`, or proceed
  deliberately (an intentional pin to an older base is legitimate; the
  `agent@rev-*` semantics exist).

**Decided: staleness never blocks.** A wrapper pinned to an older base is a
feature, not a fault; composition metadata stays visibility-only. Regeneration
still passes the one door (adapter → artifact → factory → gates) — the loop
proposes, the gates dispose.

**Open follow-up (#1221):** routing the drift signal to
`evolution-steward.default` / the operator activity feed as a *proactive*
notification, instead of relying on the planner reading the spawn note or the
roster. Deferred until there is evidence the passive surfaces are missed.

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
