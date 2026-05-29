# Constitution Restructure: Principles, Enforcement Register, Rights, and Self-Awareness

> Status: **Proposal** — feedback wanted before broad implementation.
> Epic: [#297](https://github.com/mandubian/autonoetic/issues/297).
> Phase issues: principle/register [#298](https://github.com/mandubian/autonoetic/issues/298),
> rights [#299](https://github.com/mandubian/autonoetic/issues/299),
> self-surface [#300](https://github.com/mandubian/autonoetic/issues/300),
> vocabulary [#301](https://github.com/mandubian/autonoetic/issues/301),
> detection [#302](https://github.com/mandubian/autonoetic/issues/302),
> migration [#303](https://github.com/mandubian/autonoetic/issues/303).
> Touches: the constitution itself, and Autonoetic's root documentation (`docs/ARCHITECTURE.md`, `docs/AGENTS.md`, `docs/separation-of-powers.md`, `docs/planner-principles.md`, `CLAUDE.md`, `docs/config-reference.md`) — a structural change to how the project frames its own foundations.

---

## 1. Context & motivation

Autonoetic enforces a **separation of powers**: agents are low-privilege reasoners that *propose* intents; the gateway is the high-privilege executor that *validates and runs* them. The contract between the two is the **constitution** — a versioned, Ed25519-signed artifact the gateway verifies at boot, expressing laws (rules the agent must obey) and rights (guarantees the gateway must uphold).

The model is sound. But as it has grown it has accumulated four structural problems:

1. **Rule-count intractability.** The active constitution carries ~164 enforced rules in a single flat table. Many are the *same invariant* expressed once per *implementation mechanism* (the loop guard alone is R-7.5 / R-7.7 / R-7.19 / R-7.20 — four rows for "the session halts on bounded non-progress conditions"). A flat table that mints a row per mechanism trends toward a legal code no human can hold in their head, and in which latent rule/right conflicts become undetectable.

2. **The rule-to-code gap is bridged by prose.** Each rule cites code (e.g. `guard.rs::check_loop`). These citations are hand-maintained and drift — we recently found `guard.rs:66,97` pointing at the wrong lines after an unrelated edit, and a `loop_guard.tripped` event mislabelling every trip `R-7.7`. "We have a test for every rule" is true by *discipline*, not by *construction*; nothing fails when a rule loses its test or a check loses its rule.

3. **Rights are buried.** Rights (`Ri-*`) are interleaved with rules in the same table, formally equal but visually and operationally subordinate. Agents are meant to *trust the frame*; they cannot trust guarantees they don't know they have. Today an agent mostly learns the frame by hitting denials, not by being told its entitlements.

4. **"Dumb" undersells and misdescribes the executor.** The gateway is described as a "dumb secure pipe," and the constitution tags one rule a `DUMBNESS VIOLATION`. The gateway is not dumb — it is **deterministic and discretion-free**. The folk word fights the dignity the system extends to agents through rights, and obscures the actual fault when the boundary leaks (the gateway exercising *judgment* that belongs to the agent or to pre-committed law).

A fifth observation is about purpose, not a defect. **The constitution's job is coordination, not maximal safety.** It is a Schelling point: a shared, agreed frame in which every actor can predict how the others (and the gateway) will behave, so unexpected behaviour becomes *legible* — analysable, reportable, correctable — and the amendment mechanism lets the frame *evolve* by deliberate, attributable acts. Trust comes from predictability-plus-detection, not from the impossibility of violation.

This reframing has a corollary the current design under-serves: **detection and correction are first-class.** A contract produces trust only if breaches are noticed and acted on. The project has invested more in *writing* law than in the *nervous system* that detects drift against it.

---

## 2. The reframing (vocabulary we will adopt)

| Old framing | New framing | Why |
|---|---|---|
| "Dumb secure pipe" | **Lawful Executor** — deterministic enforcement, no improvised judgment | The defining property is *zero runtime discretion*, not absence of intelligence. |
| `DUMBNESS VIOLATION` | **`DISCRETION LEAK`** (a.k.a. executor overreach) | Names the actual fault: the gateway made a judgment reserved to the agent or to pre-committed law. |
| Constitution = a list of rules | Constitution = a **coordination contract**: principles + rights, with a *generated* enforcement register | Separates the deliberated "why" from the mechanical "how." |
| Rights interleaved with rules | Rights = a **Bill of Rights**, first-class and agent-visible | An agent must know its guarantees to trust the frame. |

**Definition (Lawful Executor).** The gateway exercises *no improvised, per-case discretion*. Every judgment it makes is pre-committed at design time as signed, versioned law. Semantics — fuzzy, contextual, goal-directed reasoning — are reserved to agents. The boundary is not "no logic in the gateway"; it is "no logic the constitution didn't already authorise, deterministically."

---

## 3. Concept 1 — Principle / Enforcement Register split (the core change)

The intractability comes from flattening two different things into one signed table. Split them.

### 3.1 The Constitution proper (small, signed, deliberated)

A compact set of **principles** and **rights** — target *dozens*, not hundreds — each stating an *invariant*, not a mechanism. This is the artifact that is human-readable in one sitting, ratified, signed, and amended with ceremony.

Example collapse (loop guard): the four current rows become **one principle**:

> **P-7 (Bounded progress).** A session is halted when it stops making progress, on a closed, configurable set of mechanically-detected non-progress conditions, each emitting a typed, attributable reason. No condition relies on agent self-report.

The specific conditions (per-tool failure budget, no-meaningful-progress, rotating-poll, child-failure) are *enforcement detail*, not separate laws.

### 3.2 The Enforcement Register (large, generated, derived)

A machine-generated mapping: **principle → concrete checks → code citations → tests → config knobs**. It is *derived from the code*, not hand-legislated, and scales freely with implementation without touching the signing ceremony.

```
P-7 (Bounded progress)
 ├─ tool_failure_budget      guard.rs::register_failure+check_loop   test: …::trips_on_tool_failure_budget   cfg: loop_guard.max_tool_failures
 ├─ no_meaningful_progress   guard.rs::check_loop                    test: …::trips_on_max_loops            cfg: loop_guard.max_loops_without_progress
 ├─ rotating_polling_pattern guard.rs::register_progress_inner       test: …::rotating_polling_pattern_*    cfg: loop_guard.rotation_*
 └─ child_failure_budget     guard.rs::register_child_failure        test: …::child_failure_*               cfg: loop_guard.max_child_failures
```

### 3.3 Enforcement tagging + totality

Each enforcement point in code carries a **machine-readable tag** binding it to a principle, e.g. a macro/attribute `#[enforces("P-7", check = "rotating_polling_pattern")]` or a registered entry. The Register and the rule/right counts are *generated* from these tags.

A **totality meta-test** then makes completeness a checked invariant rather than a discipline:

- Every principle has ≥1 enforcement tag and ≥1 linked test → else CI fails.
- Every enforcement tag references a principle that exists → else CI fails.
- Every linked test exists and is not `#[ignore]` → else CI fails.

This closes the largest part of the rule-to-code gap: the citation becomes *executable and checked*, and the thing that drifted (`guard.rs:66,97`, the mislabelled event) becomes impossible to merge.

### 3.4 What the lock signs

The lock signs **the principles + rights + a digest of the generated Register**. Principles change rarely and deliberately (ceremony preserved); the Register regenerates from code on every build and its digest is verified, so signed law and live enforcement cannot silently diverge.

### 3.5 Migration (no enforcement lost)

Phased, section by section, each phase preserving every existing check:

1. Introduce the tagging mechanism + Register generator + totality meta-test, proven on **one family** (loop guard) end-to-end. *(This is the first implementation slice — see §8.)*
2. For each rule cluster, author the parent principle, tag the existing checks, generate Register entries, and confirm the meta-test stays green and the Register reproduces the old citations.
3. Replace the flat rule table with principles + a link to the generated Register. The old `R-x.y` IDs survive as Register entry keys (stable external references preserved); principles get `P-*` IDs.
4. Recompute + re-sign the lock for a new constitution version.

---

## 4. Concept 2 — Rights as first-class

Rights constrain the *gateway* on the agent's behalf. They are what makes the frame trustworthy, and they should be prominent and knowable.

1. **A distinct Bill of Rights** section at the head of the constitution, written to be surfaced into the agent's system prompt: *here is what you are guaranteed* — your reasoning is private (Ri-0.13), you will be woken rather than forced to poll (Ri-0.14), you terminate only for a closed list of reasons (Ri-0.12), you may propose amendments.
2. **Runtime-queryable rights / self.** An introspection surface so an agent can ask *"what am I guaranteed here?"* and get the constitutional answer, rather than discovering the frame through denials. (Folds into the self-awareness surface, §6.)
3. **Bind-direction on every clause.** Tag each clause by who it binds — *rule* (binds the agent) vs *right* (binds the gateway). The rights/obligations *ratio* then becomes a visible design signal; a large asymmetry is itself worth seeing and discussing.

---

## 5. Concept 3 — Lawful Executor vocabulary

Adopt the vocabulary in §2 across code and root docs:

- Rename Design Principle "Gateway as Dumb Secure Pipe" → **"Gateway as Lawful Executor: deterministic enforcement, no improvised judgment."**
- Retag `DUMBNESS VIOLATION` → **`DISCRETION LEAK`**, and document the category: a place where the gateway exercises judgment reserved to the agent or to pre-committed law. These are *tracked debts*, not acceptable behaviour.
- Sweep `docs/ARCHITECTURE.md`, `docs/separation-of-powers.md`, `docs/AGENTS.md`, `CLAUDE.md` for "dumb" phrasing.

This is vocabulary, not behaviour change — but it is structural because it edits the project's self-description at the root.

---

## 6. Concept 4 — Autonoetic self-awareness

The project's name is its thesis: *autonoetic* consciousness is self-knowing across time — past, present, and one's own continuity. The organs already exist but are scattered:

- **Past** → causal chain, digests, revision lineage.
- **Present identity** → persona, declared capabilities, and the constitution as the frame inhabited.
- **Future / evolution** → amendments, skill promotion, new revisions.

Today an agent *assembles* self-knowledge from separate tools (`agent_exists`, `digest_query`, capability inspection, `constitution_read`). Proposal: a **single first-class "self" surface** that answers, in one call:

> *Who am I* (identity, persona, revision lineage) · *what may I do* (capabilities, tool tiers) · *what am I guaranteed* (rights, §4) · *what have I done* (recent causal/digest summary) · *how do I evolve* (amendment + promotion paths).

This turns the project's thesis from an emergent property into an exercisable capability, and is the natural home to surface rights front-line.

---

## 7. Concept 5 — Detection & correction as first-class

Trust-through-predictability only holds if deviations are detected and corrected. Make the "report and correct" half a peer of the "constrain" half:

- Enforcement events (e.g. `loop_guard.tripped`, gate denials, `DISCRETION LEAK` occurrences) carry their principle ID (from §3.3) so the **sentinel / divergence monitor** can correlate breaches against the contract by principle, not by ad-hoc string.
- A standing notion of *contract health*: which principles are tripping, where discretion leaks are accumulating, whether the amendment loop is being used. This is the feedback signal that tells you when the frame needs to evolve.

---

## 8. Implementation order

1. **Principle/Register split — mechanism slice (start here).** Enforcement tag/registry, Register generator, totality meta-test, lock signs principles + Register digest. Proven end-to-end on the loop-guard family (P-7), which is well-understood. Migration of the remaining rules follows in tracked issues.
2. Rights → Bill of Rights + bind-direction tags.
3. Self / rights introspection surface.
4. Lawful-Executor vocabulary sweep.
5. Detection-loop wiring (principle-aware sentinel correlation).
6. Full rule-table migration (per-section, behind the mechanism from step 1).

Each step is a deliberate constitution version bump where it touches the signed artifact, and each updates the affected root docs.

---

## 9. Risks & open questions

- **Migration must lose no enforcement.** Mitigation: the Register must reproduce every current `R-x.y` citation as an entry key before the flat table is removed; the totality meta-test guards against orphaned checks.
- **Stable external references.** Other docs/tests reference `R-x.y` IDs. Keep them as Register entry keys so links don't break; principles take new `P-*` IDs.
- **Tagging ergonomics.** `#[enforces(...)]` must be cheap to add and impossible to forget (the meta-test enforces the latter). Open question: attribute macro vs. a central registry table vs. a hybrid.
- **What the signature covers.** Signing principles + Register *digest* (not the full generated Register text) keeps the ceremony light while still detecting code/law divergence. Confirm this satisfies the federation digest/profile checks (R+++2).
- **Right vs rule conflicts at scale.** Fewer principles makes conflicts more tractable to reason about, but the "rights win; conflict → operator review" valve still leans on humans. Consider a lint that flags a new rule whose scope overlaps an existing right.
- **Self-surface disclosure.** The unified self view must respect existing disclosure/visibility rules (it aggregates data that already has access controls; aggregation must not widen them).
