# Proposals

Design docs and RFCs with **open work**. One folder, one table — this file is
the index, and a test asserts every `.md` here appears in it.

Formerly split across `design/` and `rfc/`. The distinction was not observed:
`design/` docs described themselves as "Draft RFC" while `rfc/sandbox-mount-allow-set.md`
was an implementation spec, `rfc/` had no index at all, and 11 of 27 `design/`
docs were missing from the one index that existed.

## Status vocabulary

| Status | Means |
|---|---|
| **Open** | Not implemented, or explicitly awaiting feedback |
| **Partial** | Some phases shipped, named phases still open |
| **Validation pending** | Implemented; awaiting a measurement or operator sign-off |
| **PROMOTE** | **Shipped, but no live doc describes it.** The proposal is currently the only description, so it must be rewritten into `internals/` or `reference/` *before* being archived — archiving first would delete the documentation |
| **Discussion** | An essay, not a proposal. Nothing to implement |

A doc leaves this folder in one of two ways: **archived** with a pointer to the
live doc that now describes the behaviour, or **promoted** into that live doc
and then archived. What never happens is a shipped proposal staying here as the
de-facto reference.

## Promote first (shipped, undescribed)

**Empty — the debt is cleared.** All four shipped-but-undescribed proposals have
been promoted into live docs and archived:

| Was | Now described in |
|---|---|
| `sandbox-mount-allow-set` | [`../internals/sandbox/drivers.md`](../internals/sandbox/drivers.md) § Host-filesystem exposure |
| `unit-test-runner-divergence-loop` | [`../internals/divergence-detection.md`](../internals/divergence-detection.md) |
| `task-robustness` | [`../internals/task-survival.md`](../internals/task-survival.md) + delegation kinds in [`../reference/tool-errors.md`](../reference/tool-errors.md) |
| `content-patch-tool` | [`../reference/content-patch.md`](../reference/content-patch.md) |

The **PROMOTE** status stays in the vocabulary above: it is what a shipped
proposal gets when no live doc describes it, and the rule that produced these
four — archiving such a doc would delete the only description — applies to
whatever ships next.

## Open and partial

| Proposal | Status | Notes |
|---|---|---|
| [`citizenship-as-a-runtime-service.md`](citizenship-as-a-runtime-service.md) | Partial | Parts A–F shipped; C.3 (precision-scored civic record) deferred until C.1/C.2 accumulate adjudication volume. Tracking #844, #774 |
| [`agent-genesis-one-door.md`](agent-genesis-one-door.md) | Partial | Security core shipped (#802/#805); F.3/F.4 birth quality open (#799) |
| [`data-envelopes-egress-localization.md`](data-envelopes-egress-localization.md) | Partial | The egress master RFC. Phases 1–3 shipped; Phase 4 open — see [`egress-phase4.md`](egress-phase4.md) |
| [`egress-phase4.md`](egress-phase4.md) | Partial | Federation, MCP, sandbox, declassification (#909) |
| [`divergence-sentinel.md`](divergence-sentinel.md) | Partial | Layer 1 + manual watchdog shipped; P4 validation open. Live behaviour: [`../internals/divergence-detection.md`](../internals/divergence-detection.md) |
| [`divergence-sentinel-validation.md`](divergence-sentinel-validation.md) | Validation pending | Harness exists (`autonoetic sentinel-experiment`); awaiting operator sign-off |
| [`self-improvement-loop.md`](self-improvement-loop.md) | Partial | P0–P4 shipped (`autonoetic improve`); P5–P7 open |
| [`self-improvement-loop-validation.md`](self-improvement-loop-validation.md) | Validation pending | Awaiting a 3-cycle run |
| [`operator-activity-feed.md`](operator-activity-feed.md) | Partial | Phases 0–3 shipped (`operator_activity`, `operator.activity.list`, chat TUI); Phase 4 hardening open |
| [`operator-approval-inspection.md`](operator-approval-inspection.md) | Partial | Phase 1 (code excerpts) shipped in `approval_hardening.rs`; Phase 2 open |
| [`post-promotion-review.md`](post-promotion-review.md) | Partial | Tier 1 observational review shipped (`post_promotion_review.rs`); Tier 2 fixture drift open |
| [`constitution-restructure.md`](constitution-restructure.md) | Partial | `P-x.y` restructure in progress; see [`../constitution/enforcement-register.md`](../constitution/enforcement-register.md) |
| [`human-agent-artifact-collaboration.md`](human-agent-artifact-collaboration.md) | Partial | PlanFrame + workbench shipped (`gateway_store/plan_frames.rs`, [`../guide/human-agent-collaboration.md`](../guide/human-agent-collaboration.md)); the doc still says "not implemented" and needs re-scoping to what remains |
| [`plan-envelope-evolution.md`](plan-envelope-evolution.md) | Partial | PlanFrame landed; envelope evolution open |
| [`principal-model-and-symmetric-obligations.md`](principal-model-and-symmetric-obligations.md) | Partial | `autonoetic-types/src/principal.rs` exists; multi-decider / voting-weight / ratification vision open |
| [`operator-legibility.md`](operator-legibility.md) | Open | Tiered timeline, plan inherit/diff, approve-the-envelope, t=0 workbench |
| [`gateway-agent-divergence-robustness.md`](gateway-agent-divergence-robustness.md) | Partial | rev. 2 after code audit; umbrella #608 |
| [`gateway-determinism-and-escalation.md`](gateway-determinism-and-escalation.md) | Partial | `session_escalate` exists; feedback wanted on the rest |
| [`agent-singleton-and-spawn-dedup.md`](agent-singleton-and-spawn-dedup.md) | Open | rev. 2 draft; no `singleton_key` / `spawn_dedup` in code |
| [`agent-wiki-contributions.md`](agent-wiki-contributions.md) | **Partial** | `wiki_propose` ships (`runtime/tools/wiki.rs`) — agents can propose a new page or an edit. The PR-4a audit recorded this as Open on a bad grep (`wiki_propose` matched an unrelated file first); corrected here. Remaining: the operator review UX, #425/#426 |
| [`edit-tooling-and-guidance.md`](edit-tooling-and-guidance.md) | Open | Two intertwined tracks; roadmap |
| [`error-envelope-homogenization.md`](error-envelope-homogenization.md) | Open | Migration worklist, self-declared TODO |
| [`external-cli-agent-delegation.md`](external-cli-agent-delegation.md) | Open | Side-plan, not implemented — no `ExternalCli` in code |
| [`packager-dependency-determinism.md`](packager-dependency-determinism.md) | Open | Agreed direction, implementation scoped in the doc |
| [`remote-access-declarative-patterns.md`](remote-access-declarative-patterns.md) | Open | Phase 4.3/4.4 — move detection policy into agent-declared manifest patterns |
| [`launch-readiness-priorities.md`](launch-readiness-priorities.md) | Open | Triage view over the above |
| [`launch-presentation.md`](launch-presentation.md) | Open | Launch pitch/demo/rollout plan (#489) — messaging, not architecture; follows the `../start/concepts.md` framing |
| [`run-scoped-decider-appointment.md`](run-scoped-decider-appointment.md) | Open | "Name the night watch": appointing an agent-decider for a run as a peer principal (P-2.20 seat exists; appointment record, routing, read parity and four blocking defects) |
| [`classic-harness-usecase-validation.md`](classic-harness-usecase-validation.md) | Validation pending | Study in progress; becomes a [`../reports/`](../reports/) entry when it closes |
| [`implicit-artifacts-agent-evolution.md`](implicit-artifacts-agent-evolution.md) | Partial | Part 1 shipped; Part 2 (closed-loop evolution automation) open |
| [`agent-adaptation-composition.md`](agent-adaptation-composition.md) | Partial | Adaptation (middleware) vs composition (provenance) split. Phases 0–3 shipped (#1204, #1203, #1202, #1221): first-class `adapter` provenance, planner + self_describe discovery, roster `stale_base` + `derived_from` card, promotion-time drift event + spawn-time advisory. Open: steward/operator notification route (#1221) |

## Diagrams

[`diagrams/`](diagrams) holds the proposed pedagogical visuals for the community
and constitutional story — one interactive map plus five vector schematics:

| Asset | What it shows |
|---|---|
| [`diagrams/community-and-constitution.html`](diagrams/community-and-constitution.html) | The interactive hub: clickable bind-direction map, three lifecycle walkthroughs, the searchable clause catalog, and the schematic gallery |
| [`diagrams/constitutional-peer-blueprint.svg`](diagrams/constitutional-peer-blueprint.svg) | Hero plate, drawn as a drafting sheet: both figures dimensioned `EQ` on a common datum under the constitution seal, standing on the causal ledger, with a clause schedule and notes |
| [`diagrams/human-ai-peers.svg`](diagrams/human-ai-peers.svg) | The labelled counterpart: the four one-way bindings as arrows, plus the co-evolution cycle |
| [`diagrams/community-governance.svg`](diagrams/community-governance.svg) | The four-party governance map: served party, agents, gateway, governance seats |
| [`diagrams/diagram-functional-autonoesis.svg`](diagrams/diagram-functional-autonoesis.svg) | The six preconditions of the verified per-turn self-model |
| [`diagrams/diagram-coevolution-lifecycle.svg`](diagrams/diagram-coevolution-lifecycle.svg) | Propose → gate → ledger → promote, and the amendment track |

Two rules these assets are held to, because a diagram is read as fact:

- **Every printed clause ID must exist** in the active constitution.
  `docs_link_guard::tests::every_clause_id_in_a_diagram_resolves` scans every
  `.svg`/`.html` under `docs/` and fails on an ID the constitution does not
  declare. Use `§3` for a section and `P-*` for a family; a bare `P-3` reads as
  a clause and will fail.
- **Declared ≠ enforced.** A clause whose status is `MISSING`/`PARTIAL`/
  `DESIGN DEBT` — `U-1`–`U-3`, `I-12` — is labelled as such inline. Showing the
  §12 charter as live erases the sequencing constraint that makes it matter
  (`../concepts/philosophy.md` §5: enforce §U *before* any decider franchise
  widens).

The published site under [`../diagrams/`](../diagrams) is a different, live set
(architecture, runtime dynamics, federation, technical map) reached from
`../index.html`.

## Archived from here

Nine proposals were archived when this folder was created, each because the
behaviour it proposed is live **and** described by a live doc. Every one carries
a pointer to that doc as its first line:

`constitution-gate-amendments` → enforcement register · `agent-prompt-factorization`
→ prompt burden study · `operator-live-comments` and `session-room-conversational-input`
→ the session room guide · `promotion-completeness-invariant` → `AGENTS.md` ·
`session-room-channel-agnostic-timeline` → session room internals ·
`portable-wasm-execution-tier` → the wasm tier doc ·
`llm-preset-inference-profiles` → prompt composition ·
`credential-egress-host-authorization` → the credentials reference.

One doc moved instead of archiving: `constitutional-evolution-reflections.md` is
a discussion essay, not a proposal, and now lives at
[`../concepts/constitutional-evolution.md`](../concepts/constitutional-evolution.md).
