# Task Robustness: Typed Failures, Honest Contracts, and Fail-at-Plan-Time

> **Status:** Implemented (2026-07-13 / wiring follow-up 2026-07-19). All
> six workstreams shipped: Part A taxonomy (#783), Part B bundle (#789),
> Part C preflight (#786 + wiring PR #830), Part D burn-rate (#784),
> Part E.1 compression (#791), Part E.2 failover (#785). The design is
> kept as the canonical reference for the invariants and contract
> shape; open questions at the bottom remain open per their original
> framing.
> Tracking issue: [#781](https://github.com/mandubian/autonoetic/issues/781)
> (workstreams #775–#780). Companion to
> [`citizenship-as-a-runtime-service.md`](citizenship-as-a-runtime-service.md)
> (#774): that RFC makes agents *civically* reliable; this one makes their
> *tasks* survive. Related in-flight work: #754 (retryable LLM 5xx),
> #756/#764 (operator task retry).

---

## Motivation

The constitution now protects agents from the gateway and from each other,
but nothing protects a **task** from the mechanical ways it actually dies:
a child that declares success while producing nothing, a parent that
re-reasons about a failure from prose and gets it wrong twice, a plan whose
step 4 needs a capability nobody in the delegation chain holds — discovered
after the budget for steps 1–3 is spent, or a budget cliff reached with no
tokens left to re-plan.

The anti-bloat constraint is explicit ("common freedom without too much
bureaucracy"): everything here is gateway-side or schema-side, adds **no
per-turn prompt mass**, and mostly makes already-existing fields real
(`expected_outputs` is in the delegation contract today — and decorative).

### Invariants

1. **No blind retry against deterministic inability.** Retrying an identical
   spawn against an unmet output contract is provably the worst move — same
   input, same agent, same missing skill. Retry policies attach to *failure
   kinds*; only genuinely transient kinds retry.
2. **Fuller co-design: a contract check may only be added together with the
   exit that makes honoring it — or honestly failing it — always possible.**
   Models loop precisely when honest failure is unspeakable (the only
   permitted outcomes are "success" and "try again"). Every check below
   ships with a penalty-free typed way out. (Same grammar as P-7.19 ⟷
   Ri-0.14: the rule exists because the right makes compliance possible.)
3. **Closed enums, not prose.** A parent must be able to branch on a failure
   mechanically; re-reasoning about failure text is where a non-deterministic
   model goes wrong twice.
4. **Graduated detection, budget last.** Loops are caught by the cheapest
   detector first (loop guard on structural identity), never discovered by
   budget exhaustion — budget bounds everything but is the most expensive
   possible way to learn you were looping.
5. **Check existence, not truth.** Mechanical checks verify that declared
   outputs *exist* (resolve to content/artifact handles). Whether they are
   *good* remains the parent's / evaluator's judgment. The gateway gains no
   discretion (Lawful Executor, §14).

---

## Part A — A typed failure contract for children

### A.1 Today

A failed child returns prose. The parent re-derives what kind of failure it
was — the single most error-prone reasoning step in a delegation chain — and
retry behavior is uniform where it exists at all.

### A.2 Proposal — closed failure taxonomy

Child results carry a `failure` object with a closed `kind`:

| Kind | Meaning | Default policy |
|---|---|---|
| `transient_env` | Provider 5xx, rate limit, sandbox hiccup | Bounded retry with backoff (extends #754 to the delegation layer) |
| `capability_denied` | A needed capability was refused | **No retry.** Re-delegate / escalate — the Ri-0.3 `available_actions` affordances (#769) are the branch table |
| `input_contract` | Spawn payload failed `io.accepts` | Repair and retry once (the schema error already carries repair hints) |
| `output_contract_unmet` | Declared `expected_outputs` missing at completion (Part B) | **No blind retry.** Re-delegate, decompose, relax the contract, or escalate |
| `clarification_needed` | Child cannot proceed / cannot satisfy the contract, with its account | Parent reasons; not a sanctionable failure (invariant 2; existing [`agent-clarification.md`](../reference/agent-clarification.md)) |
| `gave_up` | Child terminated without result or account | Parent reasons; counts toward the Part B.4 loop guard |

Retry policies are per-kind, pre-committed configuration — the gateway
executes them; it does not judge (invariant 5). Parents branch on `kind`
mechanically and spend reasoning only where reasoning helps
(`clarification_needed`, `plan-shaped` failures).

## Part B — Make `expected_outputs` real, as a bundle

The delegation contract already asks parents to declare `expected_outputs`;
nothing checks them. The naive fix (check + retry) creates never-ending
loops for agents that *cannot* produce the output. The bundle below is the
loop-safe shape — **the four pieces are one design and must not ship
separately**:

### B.1 The existence check

At child completion, the gateway verifies each declared expected output
resolves to a content/artifact handle. Fires **once**; never re-invokes the
child. Existence only, never quality (invariant 5). Unmet → the result is
stamped `output_contract_unmet` with the missing names — evidence for the
parent, not a trigger for the gateway.

### B.2 No blind retry

`output_contract_unmet` defaults to no-retry (invariant 1). The parent's
mechanical options: re-delegate to a differently-skilled agent, decompose
smaller, relax the contract, or escalate. Retrying the same agent with the
same contract requires the parent to change *something* structural first
(B.4 enforces this).

### B.3 The dignified exit

A child that cannot satisfy the contract returns `clarification_needed`
with its account ("produced X instead because Y" / "this needs capability
Z") — penalty-free, first-class, cheap. This matters twice over: it is what
prevents grinding (LLMs take an offered exit far more reliably than they
invent one), and it is what disambiguates the check — `expected_outputs` is
authored by a parent that is *also* an LLM, so an unmet contract is
ambiguous between "child failed" and "contract was wrong." Only the child's
account makes that distinction cheap for the parent to draw.

### B.4 Parent-level loop guard on structural identity

The residual risk is one level up: a mediocre parent re-spawning the same
child with the same contract N times. This is the pattern the runtime
already polices (`loop_guard.tripped`, P-7.19 — repetition without new
information traced as the dominant cost in real sessions). Extend the guard:
N spawns with the same structural identity (agent alias + contract hash +
input digest — exact key is open question 3) → escalation gate with the
attempt history attached. Graduated response; a decider sees what was tried
(invariant 4).

**Why the check helps rather than hurts:** the loop it seems to risk already
exists today, silently — an agent that can't produce the output either
declares success (and the workflow fails three steps downstream, where
diagnosis costs far more) or grinds with no one counting. The bundle moves
failure to the cheapest detection point and stamps a type on it. Loops are a
symptom of *silent* failure; this is machinery for making failure loud.

## Part C — Plan-time capability preflight

### C.1 Today

Capability coverage is discovered by execution: an agent learns mid-workflow
that a step needs a capability nobody holds, after upstream budget is spent.

### C.2 Proposal

A deterministic check over a PlanFrame before execution: for each step, does
the intended agent (or any discoverable agent, via the `agent_discover`
index) declare the capabilities the step needs? Uncovered steps fail **at
plan time**, carrying the same `available_actions` affordances as a runtime
denial (#769): delegate, revise the plan, escalate, propose.

Precedent already in the codebase: `artifact_prepare` does exactly this
one-pass preflight for credentials + network domains. This lifts the pattern
from artifacts to plans. Purely static — declared capabilities vs. declared
step needs — so the Lawful Executor gains nothing to judge. Advisory first
(a plan may legitimately include steps whose executor will be *built* — the
agent-factory ladder), so the preflight *warns* with structure rather than
blocks; a planner that proceeds past a warning does so on the record.

## Part D — Burn-rate in the attestation

Ri-0.4 gives truthful *remaining* budget; robustness wants the derivative.
One computed line in the P-6.23 attestation: current burn rate and whether
the remaining plan fits the remaining budget at that rate. Pre-committed
formula (tokens/turn windowed average × remaining plan steps — refinements
are open question 5), no judgment. This converts budget exhaustion from a
cliff (P-7.18 degraded mode) into a re-planning trigger *while the agent
still has tokens to re-plan with* — the cheapest possible use of the one
channel with taught authority.

## Part E — Adjacent gaps with existing designs (referenced, not redesigned)

Two task-killers already have full designs in
[`2026-07-19-comparison-hermes-agent.md`](../reports/2026-07-19-comparison-hermes-agent.md) and need
building, not designing. Named here so the robustness picture is complete
and tracked from one place:

- **E.1 Context compression.** The top unaddressed killer for
  "runs-for-hours" sessions: token counting and pruning exist; summarization
  when approaching the limit does not. Checkpoint-anchored, opt-in per
  agent, full context always preserved on disk (audit trail untouched;
  compression affects only the in-memory window). Includes the golden-session
  quality-regression harness sketched in the design.
- **E.2 Provider failover.** `fallback_provider`/`fallback_model` exist as
  dead config; a rate limit kills a session that checkpointing then has to
  rescue. Execute the fallback chain at the driver boundary (~100 lines per
  the existing proposal). Complements #753/#754 retry work.

---

## Sequencing

1. **Part A taxonomy** — everything else stamps into it; no new tools, a
   result-shape change.
2. **Part E.2 failover** — smallest fix for the most arbitrary deaths.
3. **Part B bundle** — atomic (B.1–B.4 together, per invariant 2).
4. **Part D burn-rate** — one attestation line once plan-step estimates
   exist.
5. **Part C preflight** — advisory warnings first, on the PlanFrame surface.
6. **Part E.1 compression** — largest and riskiest (deliberately discards
   context); ships opt-in with the regression harness.

## Open questions

1. **Where `expected_outputs` semantics stop.** Existence = "resolves to a
   handle." Should a declared name be allowed to carry a minimal shape hint
   (kind: artifact vs content, non-empty)? Anything richer re-opens
   quality-judgment at the gateway — the line must stay mechanical.
2. **Contract relaxation authority.** May a parent unilaterally relax
   `expected_outputs` mid-workflow, or is that a plan amendment (PlanFrame
   revision) so the trajectory stays traceable? Leaning: revision — it is
   exactly what PlanFrame immutable-revisions exist for.
3. **Structural identity for the B.4 guard.** Agent alias + contract hash +
   input digest is strict (trivial input rewording evades it) — but a
   looser semantic key hands the gateway judgment. Start strict; measure
   evasion before loosening.
4. **One gate, two fields.** `anomalies` (#770, citizenship RFC C.2) and the
   `failure` object both land in `io.returns` validation — keep them one
   Advisory-then-Strict rollout so specialists absorb a single contract
   change, not two.
5. **Burn-rate for heterogeneous steps.** A windowed tokens/turn average
   misprices "one cheap check then one huge build." Per-role step cost
   priors from execution traces are the obvious refinement — but priors are
   config, and the formula must remain pre-committed and visible.
6. **Does `gave_up` need a right?** If `clarification_needed` is cheap and
   truly penalty-free, `gave_up` should be rare; if telemetry shows it
   isn't, the exit isn't cheap enough — that finding routes back to the
   citizenship RFC's civic-health view rather than to more enforcement here.
