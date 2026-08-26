> **Archived — shipped.** The behaviour this proposed is live and described in [`AGENTS.md`](../AGENTS.md). Kept as the design record; not source of truth.

# Promotion Completeness Invariant (fail-closed promotion)

Status: **proposed** (2026-06-05). Closes a promotion fail-open found in
production: an agent-factory revision was promoted with **no operator approval**
because the operator-approval requirement was contingent on data the orchestrator
LLM produced (recorded federation verdicts / an attached artifact). When that
data was missing, `agent_revision_promote` fell through to a "direct promote"
branch.

## Principle

The gateway is the Lawful Executor. **Whether a revision may be promoted, and
what it must satisfy, is decided mechanically by the gateway from the revision's
declared capabilities and artifact — never from orchestrator-supplied signals.**
Promotion **fails closed**: if any required aspect for the revision's risk class
is absent, promotion is **refused**, not allowed.

A single, explicit relief exists — the **cursor**: a revision that is provably
*inoffensive* may be promoted directly. "Inoffensive" is defined mechanically as
**zero declared capabilities** — such an agent cannot invoke any privileged tool
(runtime capability enforcement blocks every capability-gated call), so its blast
radius is bounded regardless of provenance. The cursor is config-tunable
(`allow_zero_capability_direct_promote`, a top-level `GatewayConfig` key, default `true`) so strictness
can be dialed up (require review even for zero-cap agents) but not silently down.

## The invariant

For a revision R of agent A being promoted:

1. **Inoffensive relief (the cursor).** If R declares **zero capabilities** and
   the cursor is enabled → direct promote. Otherwise R is *capability-bearing*
   and the rest applies.
2. **Artifact required.** A capability-bearing R must ship a reviewable artifact
   bundle. No `artifact_id` ⇒ **refuse** (incomplete — there is nothing to review).
3. **Audit / evaluation records** (existing `enforce_promotion_gate`, by class):
   - CodeExecution / AgentSpawn, or NetworkAccess + artifact ⇒ auditor PASS **and**
     an evaluator PASS, from identities distinct from each other and the proposer (P-2.17).
   - Capability-bearing intent-only bundle ⇒ auditor PASS from an identity distinct
     from the proposer.
   Missing the required promotion record ⇒ **refuse** (already fail-closed today).
4. **Operator approval is gateway-determined, not LLM-determined.** An approved
   operator escalation is required to promote when **either**:
   - federation role verdicts exist for the artifact (existing FullJury), **or**
   - R is the **first promotion of a new agent** (no outgoing revision/alias) and
     R is capability-bearing.
   Missing ⇒ **refuse** with guidance to run `federation_escalate` and obtain
   operator approval. This removes the previous dependence on the LLM having
   recorded federation verdicts before operator approval was demanded.

If all applicable aspects are satisfied → promote. If any is missing → refuse.
There is no fall-through that promotes a capability-bearing revision without these.

## First admission vs. iteration (preserving self-evolution)

The human-approval requirement is scoped to **first admission of a new agent**, not
to every promotion:

- **New agent** (no outgoing revision): capability-bearing first admission requires
  operator approval — its whole capability set is "new", i.e. maximal broadening.
  The approval can come from **either** path: the capability-acknowledgement
  approval (auto-created by the gate) **or** an **approved federation escalation**
  (the richer jury review). An approved escalation therefore satisfies the gate —
  a federation-promoted new agent is never asked to approve twice.
- **Iteration** (re-promotion of an already-admitted agent): unaffected. It is
  gated only on capability **broadening** (existing behavior) plus the always-on
  completeness gate. A non-broadening revision whose audits pass self-promotes —
  autonomous self-evolution keeps working.

The new-agent requirement is a **cursor**
(`require_operator_approval_for_new_agents`, a top-level `GatewayConfig` key, default `true`): keep it on
for human-in-the-loop admission, or turn it off to let fully-audited new agents
self-promote. Completeness stays fail-closed regardless.

## What changes vs. today

- The "no artifact OR zero-capability → direct promote" `else` is split: only the
  **zero-capability** case direct-promotes (the cursor); a capability-bearing
  revision with no artifact is **refused**.
- Operator approval (escalation) is required for the **first promotion of a new
  capability-bearing agent**, independent of whether the LLM recorded federation
  verdicts. (Re-promotion of an existing agent keeps its current gating:
  capability-delta approval on broadening + audit/eval records; this preserves
  the autonomous improvement loop for already-admitted agents.)

## Blast radius

- A brand-new capability-bearing agent (e.g. from agent-factory) can no longer be
  promoted without an approved operator escalation. The autonomous evolution loop
  must call `federation_escalate` and obtain operator approval before promoting a
  new agent — which was always the intended process.
- Zero-capability agents are unaffected (direct promote, gated by the cursor).
- Existing-agent re-promotion is unaffected.

## Constitutional note

This tightens promotion enforcement (P-2.x family). The numbered rule mapping and
any constitution wording are prepared **unsigned** for operator ratification — the
agent never signs the lock. The `enforcement_register` gains a row mapping the new
refusal (`promotion_incomplete` / new-agent operator-approval) to its rule and the
test that enforces it.
