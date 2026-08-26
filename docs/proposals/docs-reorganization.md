# Documentation reorganization — proposal

Status: **proposed** (2026-08-25) · Scope: `docs/` layout, indexes, and the
guards that keep them true · No content rewrites beyond the merges listed in §5.

---

## 1. What is actually wrong

`docs/` holds **260 files**, **137** of them live Markdown (excluding
`archived/` and the 21 frozen constitution versions) totalling **~46,600
lines**. The problem is not volume — it is that *nothing mechanically ties a
document to its place, its status, or its reader*. Measured today:

| Symptom | Evidence |
|---|---|
| The index does not index | **33 of 77** top-level docs are absent from `docs/README.md` — including `philosophy.md`, `session-room.md`, `sandbox-drivers.md`, `federation-carry-forward.md` |
| The design index does not index | **11 of 27** docs in `design/` are absent from `design/README.md` (`plan-envelope-evolution.md`, `operator-live-comments.md`, `content-patch-tool.md`, …) |
| One folder has no index at all | `docs/rfc/` — 11 RFCs, no `README.md`, not listed anywhere |
| Links rot silently | **19 dangling `docs/…` citations from live docs, agent bundles, and production Rust** — e.g. five docs citing an `approval-system.md` that moved to `archived/`, five code comments citing a moved `human-gate-unification-plan.md`, and a wiki page served to agents naming a constitution path that does not exist *and* the wrong active version. Fixed in PR 1; guarded since (§8.1). A further 147 occurrences inside `archived/` are historical records and correctly left alone |
| Two docs, one subject, neither complete | `CLI.md` + `cli-reference.md` cover **18** real subcommands between them: `security`, `watchdog`, `sentinel-experiment`, `improve` are in **neither**; `recording`/`eval`/`review` only in `cli-reference.md`; `capsule` only in `CLI.md`. The duplication is not redundancy, it is *two partial truths* |
| Status is prose, and stale | Status is declared in ≥6 formats (`**Status:**`, `Status:`, `- **Status:**`, a table cell, nothing). `rfc/portable-wasm-execution-tier.md` says "Draft (for review)" while `wasm-execution-tier.md` documents the tier as shipped |
| Kind is encoded in the filename, badly | `plan-*` (4), `spec-*` (5), `*-rfc.md` (1), `rfc-*` (1) at top level, next to `design/` and `rfc/` folders that mean the same thing |
| Dates in names, no home for dated things | `gateway-constitution-audit-2026-04-24.md`, `prompt-size-audit-2026-08-02.md` sit beside living reference docs |
| Naming is inconsistent | `ALL-CAPS.md`, `kebab-case.md`, and `snake_case.md` (`archived/plan_signal.md`) all present |

The root cause is a single missing rule: **a document's directory should tell
you what it is for, and a test should enforce that the claim is true.** Today
the directory tells you almost nothing, and nothing is enforced.

---

## 2. The organizing idea

Four axes, applied in this order:

1. **Reader intent, not subject keyword.** "I want to get something running"
   (`start/`), "I want to do a task" (`guide/`), "I need the exact contract"
   (`reference/`), "I want to know why" (`concepts/`), "I am changing the
   runtime" (`internals/`). Subject-keyword folders (`egress/`, `sandbox/`)
   only appear *inside* `internals/`, where the reader is already a contributor.
2. **Durability.** Durable truth (`reference/`, `internals/`, `concepts/`) is
   physically separated from work-in-flight (`proposals/`), from immutable
   dated artifacts (`reports/`), and from the dead (`archived/`). A reader
   should never have to date-check a doc to know whether to trust it.
3. **One canonical home per subject.** Where two docs describe one subject,
   either one is canonical and the other is a *digest that names its canonical
   source* (that is the legitimate case: `wiki/`, whose reader is an agent at
   runtime), or they merge. No silent forks.
4. **Machine-checked.** Every claim the layout makes — this link resolves, this
   wiki page has a canonical doc, this doc's status is a known value, this
   archived doc is not cited by a reference doc — is a test. This repo already
   holds the constitution digest, the enforcement register, and wiki config
   citations to that standard; docs layout should not be the exception.

---

## 3. Target tree

**Status: built in PR 2.** Every directory below exists and the top level holds
only the three entry points plus `index.html`. `design/` and `rfc/` still stand
apart — they fold into `proposals/` in PR 4, where each doc's shipped-vs-open
status decides whether it is archived, promoted, or kept (§5.1).

```
docs/
├── README.md                  ← the map. One screen. Audience-first, no per-file dump
├── ARCHITECTURE.md            ← kept in place (46 inbound refs, incl. agent bundles)
├── AGENTS.md                  ← kept in place (42 inbound refs, incl. agent bundles)
│
├── start/                     tutorials — a first success, in order
├── guide/                     how-to — one operator or author task per doc
│   └── runbooks/              procedures with a pass/fail outcome
├── reference/                 contracts you code against (CLI, config, API, schema, protocols)
├── concepts/                  why it is built this way (philosophy, powers, principles)
├── internals/                 how the runtime does it — contributor-facing
│   ├── egress/  sandbox/  prompt/  session/  storage/
├── proposals/                 in-flight design + RFC, ONE status index  (= design/ + rfc/ merged)
├── reports/                   dated, immutable: audits, validations, postmortems, studies
├── archived/                  superseded — never cited by a live doc (enforced)
│
├── constitution/              ⚠ FROZEN PATHS — loaded by the runtime
├── wiki/                      ⚠ FROZEN PATHS — agent runtime corpus
└── diagrams/  index.html  .nojekyll     published site
```

**Boundary tests** (so the next person does not have to guess):

- `reference/` vs `internals/`: *would changing this break someone outside the
  repo?* Config keys, CLI flags, HTTP routes, SQLite columns, SKILL.md fields,
  tool error envelopes → `reference/`. How the labeler resolves a sink, how the
  governor compresses → `internals/`.
- `guide/` vs `reference/`: a guide has a **goal and an order**; a reference has
  **coverage and no order**.
- `concepts/` vs `internals/`: `concepts/` survives a rewrite of the code;
  `internals/` does not.
- `proposals/` vs `internals/`: the moment a proposal ships, its *description of
  behaviour* moves to `internals/` (or `reference/`) and the proposal is
  archived with a pointer. A shipped proposal is never the live description.
- `reports/` : written once, dated in the filename, never updated. If you want
  to update it, you wanted a `guide/runbooks/` doc.

Three files stay at the top level on purpose: `README.md`, `ARCHITECTURE.md`,
`AGENTS.md`. They are the two most-linked docs in the repo plus the map, they
are cited from `CLAUDE.md`, the root `README.md`, and agent `SKILL.md` bundles
(including capsules already exported into the wild), and the ALL-CAPS
convention reads correctly in a Rust repo. Everything else moves.

---

## 4. Frozen paths — must not move

| Path | Why | Enforced by |
|---|---|---|
| `docs/constitution/versions/**` | loaded at startup by path template | `config.rs:110-113` |
| `docs/constitution/CURRENT` | active-version pointer, sync-checked | `constitution_digest.rs:856-870` |
| `docs/constitution/enforcement-register.md` | `include_str!`-compared against generated register | `enforcement_register.rs:1210` |
| `docs/constitution/recompute_lock.py` | cited in CLAUDE.md + error strings | `constitution_digest.rs:312,384` |
| `docs/wiki/**` + `index.toml` | bootstrap joins `docs/wiki`; agents call `wiki_list`/`wiki_get` | `bootstrap.rs:910`, `runtime/tools/wiki.rs:78` |
| `docs/index.html`, `docs/diagrams/*.html`, `docs/.nojekyll` | GitHub Pages | Pages build |

`docs/constitution/signing.md` and `docs/constitution/roadmap.md` are
*not* frozen but are named in Rust error strings — moving them into
`constitution/` requires editing `constitution_digest.rs:313` in the same
commit.

---

## 5. Merges and deprecations

Twelve real consolidations. "Union" means neither side is a superset — content
must be merged, not picked.

| # | Merge | Into | Why / risk |
|---|---|---|---|
| 1 ✅ | `CLI.md` + `cli-reference.md` | `reference/cli.md` | **Done.** Union, as predicted: `security`/`watchdog`/`sentinel-experiment`/`improve` were in neither, and the surviving doc advertised a `-c` short flag clap never defined. Coverage is now machine-checked (§8.2) rather than generated |
| 2 ✅ | `budget-management.md` + `session-budget.md` | `reference/budgets.md` | **Done.** Both opened by pointing at the other for the missing half |
| 3 ⚠️ | `gateway-architecture.md` + `MODULES.md` | `internals/crate-map.md` + `internals/gateway.md` | **Done differently — see §5.2.** Deduped the overlap instead of concatenating: the two docs answer different questions and one 1,128-line file would serve neither |
| 4 ✅ | `gateway-architecture-principles.md` → `separation-of-powers.md` | `concepts/separation-of-powers.md` | **Done.** Folded in as "A narrow rule enforcer, not a workflow engine" — it draws the line by *judgement* where the host doc draws it by *privilege* |
| 5 | `architecture-summary.md` | archived; ideas into `concepts/philosophy.md` | May essay framed against CCOS; superseded by `ARCHITECTURE.md` + `philosophy.md` |
| 6 | `ARCHITECTURE.md` **split** | keep overview; move session/storage chapters to `internals/` | 1,608 lines; a pasted Live Digest sample leaks `## Turn 1 — {timestamp}` headings into the document outline |
| 7 | `agent-features.md` | `AGENTS.md`, then archived | `README.md` has called it "partially superseded" since May — finish the merge (middleware / disclosure / background scheduling are the unique parts) |
| 8 | `agent-skill-frontmatter.md` + `AGENTS.md` SKILL section | `reference/skill-manifest.md` | Three descriptions of one manifest (+ the `wiki/skill-manifest.md` digest). 0 inbound refs on the top-level file |
| 9 ❌ | ~~`security-sentinel.md` + `divergence-sentinel-design.md`~~ | — | **Withdrawn (§5.4).** Two unrelated subsystems share the word "sentinel"; there was nothing to merge. Divergence detection got the live doc it lacked: `internals/divergence-detection.md` |
| 10 | `session-room.md` / `session-room-architecture.md` / `rfc/session-room-channel-agnostic-timeline.md` / `design/session-room-conversational-input.md` | `guide/session-room.md` + `internals/session/room.md`; RFC + input plan archived | User guide vs architecture split is *good* — keep it, drop the two shipped design docs |
| 11 | `spec-capability-driven-sandbox-isolation.md` | `internals/sandbox/isolation.md` (spec archived) | April spec, 97 lines, 23 inbound refs → sweep required |
| 12 | `plan-egress-phase2-907.md`, `plan-egress-phase3-908.md` | archived; live model → `internals/egress/` | Phases merged. Phase 4 (#909) stays a proposal while slices are open |

### 5.2 Deviation: merge #3 deduped rather than concatenated

The plan called for folding `MODULES.md` into `gateway-architecture.md` as one
`crate-map.md`. Implementing it showed that wrong. The two docs state their
scopes explicitly and they differ: one is "developers navigating **this crate**"
(`autonoetic-gateway/src/` only), the other is "**all** Autonoetic system
modules". They answer *where does X live across the workspace?* and *how does
the gateway work inside?* — and concatenating them produces a 1,128-line file
that answers neither well.

What was actually duplicated is narrower: **a gateway module map existed twice**,
once as an annotated ASCII tree and once as per-file tables, free to drift. So:

- `internals/gateway.md` — the gateway crate in depth (was
  `gateway-architecture.md`), and the **only** per-file gateway module map;
- `internals/crate-map.md` — the workspace map (was `MODULES.md`), whose
  duplicated gateway tables (107 lines) collapse to a crate-level summary plus
  a pointer. HTTP endpoints now point at `reference/http-api.md`, which already
  specified them.

The rule this suggests for the remaining merges: **merge information, not
files.** Two docs with genuinely different audiences should keep their
separation; what must be single is each *fact*.

### 5.3 What the audit found about self-declared status

Statuses in the docs' own headers were wrong often enough to be useless as an
input. Measured across the 39 docs:

- `rfc/portable-wasm-execution-tier.md` said "Draft (for review)" while the tier
  is shipped and documented in `internals/sandbox/wasm-tier.md`;
- `rfc/session-room-channel-agnostic-timeline.md` said "Draft" while the room
  and its canonical timeline are live;
- `design/human-agent-artifact-collaboration-plan.md` said "Not implemented"
  while PlanFrame + workbench ship in `gateway_store/plan_frames.rs` and are
  documented in `guide/human-agent-collaboration.md`;
- `design/promotion-completeness-invariant.md` said "proposed" while the
  invariant ships as `auditor_critical_veto` in `runtime/tools/promotion.rs`;
- conversely `design/task-robustness.md` said "Implemented" while two of its
  parts are described in no live doc at all.

So the audit ignored the headers and probed twice per doc: **is the behaviour in
the code?** (grep for the named symbol) and **does a live doc have it as a
subject?** (not merely mention it — `reference/config.md` listing `allow_set`
is not a description of declared mounts). Only both-yes archives.

That is also why §8.3's `live_reference` invariant matters more than a `status`
field: a status is a claim its author never has to keep true, while a
`live_reference` that must resolve is checkable.

### 5.4 Correction: two subsystems share the word "sentinel"

PR 2 renamed `security-sentinel.md` → `internals/divergence-sentinel.md`, and
merge #9 in §5 proposed folding `design/divergence-sentinel-design.md` into it.
That was wrong: the file describes the **security sentinel** (security findings,
triage, supply chain, red team; `src/sentinel/`), while divergence detection is a
different subsystem (session trajectory, LoopGuard pressure, the watchdog;
`runtime/trajectory_health.rs`). Same word, unrelated machinery.

Found while looking for the promotion target for
`unit-test-runner-divergence-loop`: the intended target's own heading said
"Security Sentinel: System-Tier Security Agent". The lesson is narrow but real —
**a rename is a claim about content, and merge #9 was proposed from filenames
rather than from reading both files.**

Fixed here: the file is `internals/security-sentinel.md` again, and divergence
detection got the live doc it never had
(`internals/divergence-detection.md`), which is where the promotion landed.
Merge #9 is withdrawn: there is nothing to merge, because the two docs were
never about the same thing.

### 5.5 Promotions describe as-built, not as-proposed

Writing the four promotions surfaced a pattern worth stating: **the shipped
design was routinely better than the proposal, and the promoted doc must follow
the code.**

`task-robustness` proposed a `failure` object with a closed six-value `kind`.
What shipped is different and better: the delegation cases fold into the existing
`FailureClass` enum (`OutputContractUnmet`, `ChildGaveUp`, `BadReference`) carried
on `ToolError.failure_class`, and retry policy moved to a separate `RetryAdvice`. Keeping *what happened* apart from
*what may be done about it* is what lets a parent branch on one field — a
distinction the proposal did not draw. A promotion that copied the proposal's
table would have documented an API that does not exist.

This is also why promotion is authorship rather than a move: the split of
`task-robustness` across two docs (`internals/task-survival.md` for the runtime
machinery, `reference/tool-errors.md` for the envelope facts) follows §5.2's rule
— merge information, not files — and neither half is a copy of the proposal.

### 5.6 Guards came from defects, not from foresight

Every check in `docs_link_guard` was written *after* something slipped through,
and each one was invisible to the checks that already existed:

| Check | Written because | Found immediately |
|---|---|---|
| `docs/…` citations resolve | months of silent rot | 19 dangling, incl. a wiki page telling agents the wrong constitution version |
| relative links resolve | PR 2 would move 75 files | 8 already broken |
| link labels match targets | review caught 5 | 24 total |
| every proposal indexed | the old index missed 11 of 27 | — |
| anchors resolve | merge #6 cannot be done safely without it | 0 (preventive) |
| documented symbols exist | review caught a **`ToolErrorKind` that does not exist** | 10 candidates, all legitimate — then review caught the check itself being vacuous (§5.7) |

The pattern worth keeping: each guard was cheap only because the defect had
already been paid for. The ordering discipline that made it work is *guard
first* — extend the check before the change that would break things, not after.
PR 2 and PR 5 both did that; the label defect is what happens when you don't.

Two of these came from a reviewer, not from me. The symbol check exists because
review noticed I had documented a type that was never in the codebase — in the
same commit whose §5.5 argued that promotions must follow the code.

### 5.7 A guard can be vacuous, and only a negative test shows it

Review on PR 5 pointed out that `documented_symbols_exist` checked only the
last `::` segment: `NoSuchType::run` would pass on the strength of some
unrelated `run`. Correct, and tightening it (literal token first, then **every**
segment as a whole word) cost nothing — measured: 63 qualified citations, 0
newly failing.

Fixing it exposed something worse. The negative test still passed — because the
haystack was built from whole Rust files, and this guard's **own unit-test
fixture** contains the string `NoSuchType::run`. The check was defining its
counter-example into existence. Any symbol named in any test fixture anywhere in
the workspace counted as real.

The fix was already in the module: `production_prefix`, written for the citation
check so test fixture *paths* would not be treated as citations. The same
reasoning applies to symbols and I did not apply it — the haystack now uses it.

Two lessons, both cheap and both learned late:

- **Assert the failure, not just the pass.** Every guard here was negative-tested
  by injection, which is the only reason this surfaced.
- **A guard that reads the whole repo will read its own fixtures.** Self-scanning
  tools need to exclude their own test data, or they answer "yes" to everything
  they were built to catch.

### 5.1 Shipped ≠ archivable: promote when the plan is the only description

A shipped design doc must **not** be archived reflexively. Archiving is correct
only when a live doc already describes the behaviour; where the plan is the
*only* description, archiving it deletes the documentation. Such a doc gets
**promoted**: rewritten from plan-voice into a present-tense description under
`internals/` (or `reference/`), and only then is the plan record archived.

The test is not keyword presence — `config-reference.md` listing a config key is
not a description of the subsystem that key configures. The test is: **could a
reader learn this subsystem from a live doc?** Three outcomes per doc:

| Outcome | When | Action |
|---|---|---|
| **Archive** | a live doc describes it | move, add `superseded_by:` |
| **Promote** | only the plan describes it | rewrite into `internals/`, then archive the plan record |
| **Split** | live doc covers part | extract the uncovered sections into the live doc, archive the rest |

Preliminary pass (full audit is PR 4 — these are candidates, not verdicts):

| Doc | Live coverage found | Provisional |
|---|---|---|
| `design/task-robustness.md` | `expected_outputs` mentioned in `ARCHITECTURE.md`/`AGENTS.md`/`config-reference.md`; cross-provider failover described **nowhere**; Part E.1 covered by `context-compression.md` | **Split → promote** the failure taxonomy + failover + preflight |
| `rfc/sandbox-mount-allow-set.md` | only `config-reference.md` names `mount_set`; declared mounts + operator allowlist + tier guard undescribed (shipped #1002 this week) | **Promote** into `internals/sandbox/drivers.md` |
| `rfc/unit-test-runner-divergence-loop.md` | one passing mention in the beginners guide | **Promoted** into `internals/divergence-detection.md` (new — the security sentinel doc was the wrong target, §5.4) |
| `rfc/credential-egress-host-authorization.md` | `credential-management.md` documents `allowed_hosts` | Archive — **verify** it covers routing-input-not-bypass semantics |
| `rfc/llm-preset-inference-profiles.md` | `config-reference.md` + `wiki/gateway-config.md` cover presets as config | Archive — **verify** the inference-profile concept is covered |
| `design/promotion-completeness-invariant.md` | severity gating in `AGENTS.md`/`CLAUDE.md`/`agent-prompt-guidance.md` | Archive — **verify** the invariant itself is stated |
| `design/plan-envelope-evolution.md` | PlanFrame described in `human-agent-collaboration.md` | Archive (open items → `proposals/`) |
| `rfc/portable-wasm-execution-tier.md` | `wasm-execution-tier.md` | Archive |
| `rfc/session-room-channel-agnostic-timeline.md` | `session-room-architecture.md` | Archive |
| `design/constitution-gate-amendments.md` | constitution §2 + `constitution/enforcement-register.md` | Archive |
| `design/agent-prompt-factorization.md` | `prompt-burden-study.md` + `agent-prompt-guidance.md` | Archive |
| `design/operator-live-comments.md` | `session-room.md` §Commenting on live files | Archive |
| `design/session-room-conversational-input.md` | `session-room.md` §Input prompts | Archive |

This is also the strongest argument for §8.3's rule that a shipped proposal must
name a `live_reference` that exists: a doc that cannot name one is a promote
candidate, and the guard finds it mechanically instead of by review.

**No redirect stubs.** Refs are swept in the same PR as the move and the §8.1
guard makes a missed ref a build failure. Exception: paths named in Rust error
strings and in `agents/**/SKILL.md` are updated in that same PR by necessity.

---

## 6. Mapping — every live doc gets a destination

### 6.1 `reference/` — contracts

| From | To |
|---|---|
| `CLI.md` + `cli-reference.md` | `reference/cli.md` (merge #1, generated) |
| `config-reference.md` | `reference/config.md` |
| `remote-agents-http-api.md` | `reference/http-api.md` |
| `gateway-store-schema.md` | `reference/store-schema.md` |
| `agent-skill-frontmatter.md` | `reference/skill-manifest.md` (merge #8) |
| `tool-error-contract.md` | `reference/tool-errors.md` |
| `response-validation-gate.md` | `reference/response-contract.md` |
| `agent-messaging.md` | `reference/agent-messaging.md` |
| `agent-clarification-protocol.md` | `reference/agent-clarification.md` |
| `agent-discovery.md` | `reference/agent-discovery.md` |
| `agent-adapter-specialist.md` | `reference/agent-adapter-contract.md` |
| `protected-agents.md` | `reference/protected-agents.md` |
| `principal-seat-capability.md` | `reference/principal-seat-capability.md` |
| `plan-capability-grants.md` | `reference/capability-grants.md` (drop `plan-`) |
| `revision-signing.md` | `reference/revision-signing.md` |
| `credential-management.md` | `reference/credentials.md` |
| `budget-management.md` + `session-budget.md` | `reference/budgets.md` (merge #2) |
| `scheduled-tasks.md` | `reference/scheduled-tasks.md` |
| `schema-enforcement-hook.md` | `reference/schema-enforcement.md` |

### 6.2 `start/` and `guide/`

| From | To |
|---|---|
| `autonoetic-concepts-for-beginners.md` | `start/concepts.md` |
| `quickstart-planner-specialist-chat.md` | `start/planner-specialist-chat.md` |
| `session-room.md` | `guide/session-room.md` |
| `session-forking.md` | `guide/session-forking.md` |
| `human-agent-collaboration.md` | `guide/human-agent-collaboration.md` |
| `agent-learning.md` | `guide/agent-learning.md` |
| `remote-access-approval.md` | `guide/remote-access-approval.md` |
| `fts-session-search.md` | `guide/session-search.md` |
| `cognitive-capsule.md` | `guide/cognitive-capsule.md` |
| `civic-eval-measurement-runbook.md` | `guide/runbooks/civic-eval-measurement.md` |
| `iteration-repair-validation-runbook.md` | `guide/runbooks/iteration-repair-validation.md` (March — verify or archive) |

### 6.3 `concepts/`

| From | To |
|---|---|
| `philosophy.md` | `concepts/philosophy.md` |
| `separation-of-powers.md` + `gateway-architecture-principles.md` | `concepts/separation-of-powers.md` (merge #4) |
| `planner-principles.md` | `concepts/planner-principles.md` |
| `design/constitutional-evolution-reflections.md` | `concepts/constitutional-evolution.md` (a discussion, not a proposal) |
| `architecture-summary.md` | → `archived/` (merge #5) |

### 6.4 `internals/`

| From | To |
|---|---|
| `gateway-architecture.md` + `MODULES.md` | `internals/crate-map.md` (merge #3) |
| `ARCHITECTURE.md` §Checkpoints/Causal chain/Event store/Digest | `internals/session/lifecycle.md` (split #6) |
| `ARCHITECTURE.md` §Content storage/Unified DB/Read cache | `internals/storage/overview.md` (split #6) |
| `session-room-architecture.md` | `internals/session/room.md` |
| `workflow-orchestration.md` | `internals/workflow-orchestration.md` |
| `content-store.md` | `internals/storage/content-store.md` |
| `content-visibility.md` | `internals/storage/content-visibility.md` |
| `spec-artifact-content-identity-model.md` | `internals/storage/artifact-identity.md` |
| `prompt-budget.md` | `internals/prompt/budget.md` |
| `context-compression.md` | `internals/prompt/context-compression.md` |
| `agent-prompt-guidance.md` | `internals/prompt/composition.md` |
| `prompt-burden-study.md` | `internals/prompt/burden-study.md` |
| `sandbox-drivers.md` | `internals/sandbox/drivers.md` |
| `sandbox-network-grant.md` | `internals/sandbox/network-grant.md` |
| `network-sink-detection.md` | `internals/sandbox/sink-detection.md` |
| `wasm-execution-tier.md` | `internals/sandbox/wasm-tier.md` |
| `spec-capability-driven-sandbox-isolation.md` | `internals/sandbox/isolation.md` (merge #11) |
| `egress-data-owner-compartment.md` | `internals/egress/data-owner-compartment.md` |
| *(new)* | `internals/egress/README.md` — the current label plane, distilled from the RFC + shipped phases |
| `security-sentinel.md` | `internals/security-sentinel.md` (merge #9 withdrawn — §5.4) |
| `federation-carry-forward.md` | `internals/federation-carry-forward.md` |
| `code-analysis.md` | `internals/code-analysis.md` |
| `observability-redaction.md` | `internals/observability-redaction.md` |
| `approved-resources-caching.md` | `internals/approval-cache.md` |
| `approval-notification-delivery.md` | `internals/approval-delivery.md` |
| `spec-install-pipeline-hardening.md` | `internals/install-pipeline.md` |
| `spec-build-layers-dependency-resolution.md` | `internals/build-layers.md` |

### 6.5 `constitution/` (human docs joining the frozen corpus)

| From | To |
|---|---|
| `constitution-signing.md` | `constitution/signing.md` — **also edit** `constitution_digest.rs:313` |
| `gateway-constitution-roadmap.md` | `constitution/roadmap.md` |
| `design/constitution-restructure.md` | `proposals/constitution-restructure.md` (still partial) |

### 6.6 `reports/` — dated, immutable

| From | To |
|---|---|
| `gateway-constitution-audit-2026-04-24.md` | `reports/2026-04-24-constitution-audit.md` |
| `prompt-size-audit-2026-08-02.md` | `reports/2026-08-02-prompt-size-audit.md` |
| `comparison-hermes-agent.md` | `reports/2026-07-19-comparison-hermes-agent.md` |
| `postmortems/session-b6d27af2-weather-agent.md` | `reports/postmortems/…` (unchanged content) |
| `design/divergence-sentinel-validation.md` | `reports/…` once signed off; `proposals/` until then |
| `design/self-improvement-loop-validation.md` | same rule |
| `rfc/classic-harness-usecase-validation.md` | `reports/…` when the study closes |

### 6.7 `proposals/` — `design/` + `rfc/` merged, one index

Keep as proposals (open work, per `design/README.md` + own headers):
`citizenship-as-a-runtime-service`, `agent-genesis-one-door`,
`human-agent-artifact-collaboration`, `operator-legibility`,
`principal-model-and-symmetric-obligations`, `constitution-restructure`,
`operator-approval-inspection`, `operator-activity-feed`,
`post-promotion-review`, `self-improvement-loop`, `plan-envelope-evolution`,
`error-envelope-homogenization-worklist`, `edit-tooling-and-guidance-roadmap`,
`external-cli-agent-delegation`, `packager-dependency-determinism`,
`content-patch-tool`, `agent-wiki-contributions`,
`rfc/agent-singleton-and-spawn-dedup`,
`rfc/gateway-agent-divergence-robustness`,
`rfc/gateway-determinism-and-escalation`, `rfc/sandbox-mount-allow-set`,
`rfc/data-envelopes-egress-localization`, `plan-egress-phase4-909`,
`phase4-declarative-patterns-rfc` → `proposals/remote-access-declarative-patterns.md`,
`rfc-launch-readiness-priorities` → `proposals/launch-readiness-priorities.md`.

Naming: drop `plan-`, `spec-`, `rfc-` prefixes and the `-plan` / `-design` /
`-rfc` suffixes — the folder already says it. Keep the issue number only where
it is the actual identifier (`egress-phase4` cites #909 in front matter, not in
the filename).

**Why merge `design/` and `rfc/`?** The distinction is not observed today:
`design/human-agent-artifact-collaboration-plan.md` and
`design/principal-model-…` self-describe as "Draft RFC", while
`rfc/sandbox-mount-allow-set.md` is an implementation spec. Two folders, two
indexes, one of them missing entirely, neither authoritative. One folder with
one status table is strictly better. *(Alternative, if you prefer to keep the
split: `proposals/rfc/` for "should we?" and `proposals/plans/` for "how?" —
but then a doc must be moved between them when it changes phase, and nothing
enforces that.)*

### 6.8 `wiki/` — frozen, but given a contract

Seven wiki pages share a basename with a top-level doc (`agent-messaging`,
`credential-management`, `remote-access-approval`, `workflow-orchestration`,
`approval-system`, `agent-capabilities`, `architecture-overview`). This is
**legitimate duplication** — a wiki page is a short digest served into an
agent's context, a docs page is the human reference — but nothing says which is
canonical, so both drift. Fix without moving anything:

- add `canonical = "docs/reference/agent-messaging.md"` to each `[[pages]]`
  entry in `index.toml` (parsed already, never served to agents, so no prompt
  cost);
- extend the existing `runtime/tools/wiki.rs` tests: every page has a
  `canonical` that resolves, and no page exceeds a line budget (they are
  digests — currently 46–162 lines, so ~180 is a safe ceiling);
- state the rule in `wiki/README.md`: **the wiki never states a fact its
  canonical doc does not.**

---

## 7. Conventions

1. **kebab-case** everywhere except `README.md`, `ARCHITECTURE.md`,
   `AGENTS.md`. `archived/` `snake_case` names get renamed in the same sweep.
2. **No kind prefixes/suffixes in filenames** (`plan-`, `spec-`, `-rfc`,
   `-plan`, `-design`) — the directory carries the kind.
3. **No dates in filenames outside `reports/`**, where the date leads:
   `YYYY-MM-DD-slug.md`.
4. **Front matter** on every doc outside the frozen dirs:

   ```yaml
   ---
   status: reference | guide | concept | internals | proposal | report | superseded
   updated: 2026-08-25
   # proposal only — where the shipped behaviour is described:
   live_reference: docs/internals/sandbox/drivers.md
   # superseded only:
   superseded_by: docs/internals/sandbox/drivers.md
   ---
   ```

   Not in `wiki/` (whole file is served to agents — use `index.toml`) and not
   in `constitution/versions/**` (digest-signed bytes).
5. **`docs/README.md` becomes a map, not a catalogue** — one paragraph per
   directory plus the five entry points. Per-file lists go stale; the directory
   listing is the catalogue.

---

## 8. Enforcement — the part that makes it stick

Three guards, cheapest first. Without these, the layout decays exactly as the
current one did.

### 8.1 Link resolution test (blocks dangling refs from ever returning) ✅ shipped

`autonoetic-gateway/src/docs_link_guard.rs` — scans every `.md` outside the
frozen/historical corpora plus the production prefix of every `.rs` file, and
asserts each cited `docs/…` path exists. Same spirit as
`every_parseable_citation_resolves`.

It is a **lib** unit test, not an `autonoetic-gateway/tests/` one and not an
`xtask`: PR CI runs `cargo nextest run --workspace --lib --bins`, and the
`tests/` binaries are only *compiled* per-PR (they run nightly). A guard in
`tests/` would not gate a PR — which is the whole point. Copy this placement
for any future repo-hygiene guard.

Citations count when they end in `.md`/`.toml`/`.json`/`.py` **or** name an
extensionless pointer file by the uppercase convention (`docs/constitution/CURRENT`,
cited 24 times and sync-checked by the runtime). The rule is the uppercase
convention rather than "no extension" because accepting any extensionless path
flags prose fragments and line-wrapped paths.

**Relative links are checked too** (`every_relative_markdown_link_resolves`,
added in PR 2). The `docs/…` citation check has a blind spot that only shows up
under a reorganisation: most intra-docs navigation is written *relatively*, with
targets like `./agent-learning.md` or `../design/README.md` — 246 such links
inside `docs/` — and such a link breaks when **either** endpoint moves. A
guard that only checked `docs/…` citations would have declared the move clean
while shredding navigation. Turning it on before moving anything also surfaced
**8 relative links already broken on `main`**, invisible to PR 1's check:
`ARCHITECTURE.md` → a moved interaction-answering plan,
`agent-clarification-protocol.md` → `foundation_instructions.md` (split into
per-layer `foundation_*.md` files by the prompt-burden work),
`prompt-budget.md` → an archived `agent-capabilities.md`, and five more.

### 8.2 Generated CLI reference

`reference/cli.md` generated from the clap command tree by an `xtask`, with a
CI check that the committed file matches. This is the mechanical fix for the
class of defect found in §1 — two hand-written CLI docs that between them miss
four commands. Hand-written prose (workflows, examples) lives in
`guide/`, which the generator never touches.

### 8.3 Status invariants — what shipped, and one that was wrong

Shipped:

- **`proposals/README.md` lists every proposal** (PR 4a). The index it replaced
  missed 11 of 27 and its `rfc/` sibling had none.
- **Every wiki page names a `canonical` doc that exists**, and pages stay under
  a 200-line digest budget (PR 5). `canonical` lives in `index.toml`, not the
  page body, so it costs no prompt tokens.
- **Anchors resolve** (PR 5) — the third invisible half of a link, and the
  precondition for merge #6.
- **Documented symbols exist** (PR 5) — added because review caught three
  API-accuracy errors no guard could see (§5.6).

**Withdrawn: "an `archived/` doc is not linked from a live doc."** Sound in the
abstract, wrong here. The promotions in 4b/4c *deliberately* point live docs at
archived design records as provenance — `internals/task-survival.md` links
`archived/task-robustness.md` for the open questions and the contract shape's
origin. That is the archive doing its job. The distinction that matters is
whether the archived doc is cited *as the description* or *as the record*, and
that is not mechanically decidable, so the rule cannot be a test. What replaces
it: promotions stamp the archived doc with a pointer to the live one, so a
reader who lands there is redirected in one line.

**Not done: front-matter `status` on every doc.** With `proposals/README.md` as
the single status table, per-file front matter would be a second place to keep
the same fact true — and §5.2's rule says merge information, not files. Left
undone deliberately rather than deferred.

---

## 9. Migration — five PRs, each independently reviewable

| PR | Content | Risk |
|---|---|---|
| **1. Guard first** ✅ **done** | Added `docs_link_guard` (§8.1) as a **lib** unit test — PR CI runs `--lib --bins` only, so a guard in `tests/` would not gate a PR — plus `docs/.link-guard-allow`; fixed all 19 dangling citations. **No moves.** | None — pure repair, and it becomes the safety net for everything after |
| **2. Move, don't edit** ✅ **done** | Guard extended to relative links **first** (§8.1), then 75 `git mv`s and a link rewrite across 138 files. The rewrite is a resolver, not a sed script: a relative link is recomputed whenever *either* endpoint moves, so all four cases (both static, source moved, target moved, both moved) are handled. `docs/README.md` rewritten as the map — pulled forward from PR 5 because every line's path changed anyway and shipping a stale catalogue would be worse | Medium — large diff, mechanically verified by both guard checks |
| **3. The real merges** ✅ **partly done** | Shipped: #1 CLI union (+ a coverage guard so the omission class cannot recur), #2 budgets, #3 crate map (deduped — §5.2), #4 powers. **Deferred with reasons below:** #6 ARCHITECTURE split, #7 agent-features fold, #8 skill manifest, #9 sentinel, #11 sandbox isolation | Content review needed — one commit per merge |
| **4a. Status audit + `proposals/`** ✅ **done** | All 39 design+RFC docs audited against code and against live-doc coverage. 29 → `proposals/` under one index, 9 → `archived/` each stamped with a pointer to the doc that now describes it, 1 → `concepts/` (a discussion essay, not a proposal). `design/` and `rfc/` are gone. New guard: every proposal must be linked from `proposals/README.md` | Done — judgement per doc, recorded in the index |
| **4b/4c. Promotions** ✅ **done (4 of 4)** | `sandbox-mount-allow-set` → `internals/sandbox/drivers.md`; `unit-test-runner-divergence-loop` → new `internals/divergence-detection.md`; `task-robustness` → new `internals/task-survival.md` + delegation kinds into `reference/tool-errors.md`; `content-patch-tool` → new `reference/content-patch.md`. All four archived with pointers. 4b also corrected a PR-2 error — see §5.4 | Content authorship — one doc at a time |
| **5. Contracts** ✅ **done** | `wiki/index.toml` `canonical` field + digest budget (§6.8), anchor resolution, documented-symbol existence (§5.6), `docs/index.html` pointing at the map. CLI coverage was solved by checking rather than generating in PR 3 (§8.2); one proposed invariant is withdrawn with reasons (§8.3) | Low |

PR 1 and PR 5 are valuable even if 2–4 are never merged.

---

### 9.1 What PR 3 deferred, and why

PR 3 took the merges where two documents stated the *same fact twice*. The rest
are restructurings or classifications, each with a reason to wait:

| Merge | Why not yet |
|---|---|
| #6 `ARCHITECTURE.md` split | 1,608 lines and the highest-fan-in doc in the repo. Splitting moves anchors, so inbound `#section` links break in ways neither guard check can see (an anchor is not a path). Wants an anchor check first — a natural PR 5 addition |
| #7 `agent-features.md` → `AGENTS.md` | A 434-line fold into the canonical agent reference, which is also cited from agent bundles and prompt-composition tests. Its unique material (middleware, disclosure, background scheduling) deserves a careful read against current code, not a paste |
| #8 skill manifest | `reference/skill-manifest.md` already exists post-rename; folding `AGENTS.md`'s SKILL.md section into it touches the doc agents read at runtime, so it belongs with #7 |
| #9 sentinel | The live doc must absorb `design/divergence-sentinel-design.md`, which is `design/` — that is PR 4's territory, and PR 4 decides whether it is archived or promoted first |
| #11 sandbox isolation | Same shape: needs the shipped-status verdict on `rfc/sandbox-mount-allow-set.md` (§5.1 flags it as promote-not-archive) before deciding what `internals/sandbox/isolation.md` should contain |

The dependency is real, not scheduling preference: #9 and #11 cannot be done
correctly before the PR 4 audit, because archiving a design doc that is the only
description of shipped behaviour would delete the description (§5.1).

## 10. Decisions

Settled by the operator, 2026-08-25:

1. **Merge `design/` + `rfc/` into `proposals/`** — yes, *with* the §5.1 check:
   before archiving anything as shipped, confirm a live doc actually describes
   the behaviour. Where none does, the doc is promoted into `internals/`, not
   archived.
2. **`ARCHITECTURE.md` and `AGENTS.md` stay at the top level** — highest fan-in
   in the repo, and cited from capsules already exported elsewhere.
3. **Execution order: PR 1 only for now** — guard + reference repair, no moves.

Still open:

4. **Generate the CLI reference?** Recommended (§8.2). It is the only doc in the
   tree whose truth is fully derivable from code, and the only one where the
   duplication has already cost coverage.
5. **Front matter, or a single `> **Status:**` line?** Front matter parses
   cleanly and is invisible in rendered Markdown; the status line needs a regex
   and the six existing formats have to be normalised either way.
6. **`archived/` — keep, or prune to git history?** 62 files. Keeping them is
   cheap and they are cited by postmortems; the §8.3 no-live-citation rule is
   what contains the cost. Recommendation: keep, enforce the rule.
