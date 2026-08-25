# Principal, Seat, and Capability

> **Status:** reference. A precise definition of the three orthogonal axes
> the gateway uses to describe *who* is acting, *in what function*, and
> *with what permission*. Several documents (the constitution, the
> principal-model RFC, `docs/AGENTS.md`) and the code assume this
> vocabulary; this page is the single place it is pinned down. It exists
> because the three are easy to conflate — conflating them once produced a
> bug where a served-user decision was silently reattributed as a script,
> and a prose claim that "`GateDecider` is a seat."

## The one-line mnemonic

> **A capability is a key. A seat is the room the key opens. A principal is
> who walks through the door.**

## The three axes

| Axis | Question it answers | Code type | Examples |
|---|---|---|---|
| **Principal** (identity + kind) | *Who* is acting — and what *kind* of party? | `Principal { kind, id }` where `kind: PrincipalKind` | `Human` ("operator"), `AutonoeticAgent` ("agent:auditor.default"), `ServedUser` ("user:alice"), `ForeignAgent`, `Script` |
| **Seat** (role / function) | *Which function* is the principal performing in this act? | `SessionRole` | `Operator` (the deciding seat), `Planner`, `Specialist { kind: "coder" }`, `Sentinel`, `Auditor`, `Runtime` |
| **Capability** (permission) | *May* this principal do this specific thing? | `Capability` | `GateDecider { kinds }`, `NetworkAccess { hosts }`, `CodeExecution { patterns }`, `ConstitutionalProposal` |

These are **independent**: any principal kind may (if granted the
capability) occupy any seat the capability authorizes. A human and an agent
can both hold `GateDecider` and decide the same gate — from different
seats, as different kinds, with the same duty.

## How to tell them apart

The test for which axis something belongs on:

- If the question is **"may this principal do X?"**, it is a *capability*.
  Capabilities are checked at a chokepoint and return allow/deny with a
  rule (`policy.rs`). They carry scope data (which hosts, which gate kinds)
  — that is permission shape, not role.
- If the question is **"what function is this principal performing by doing
  X?"**, it is a *seat*. A seat is the office occupied for the duration of
  an act; it is how the act is attributed and rendered.
- If the question is **"who, and what kind of party?"**, it is a *principal*.
  Kind is orthogonal to authority — see the rules below.

## The rules that make them compose

1. **Kind is orthogonal to authority.** A principal is not more privileged
   *because* it is `Human`; humans are sovereign over the *frame*
   (amendments, escalation), but in day-to-day interaction a `human`
   principal and an `ai` principal are bound by the same obligations and
   owed the same rights (`docs/design/principal-model-and-symmetric-obligations.md`
   Part A). This is what makes "office before occupant" real: the *duty*
   attaches to the act and the seat, not to the kind of the occupant.

2. **Capabilities compose; seats do not.** A principal may hold many
   capabilities at once (`NetworkAccess` *and* `GateDecider` *and*
   `CodeExecution`), but it occupies one seat at a time. This is why
   `GateDecider` is a capability, not a `SessionRole` variant: "the kinds
   of gate I may decide" is permission data, and a principal can be both a
   gate-decider and a planner in the same session.

3. **Duties attach to the act, via the seat — never to the kind.** O-1
   (motivated decision) and O-2 (attribution) bind *whoever decides a gate*,
   whether that principal is `Human` or `AutonoeticAgent`. The constitution
   keys accountability off `principal.kind` (to tell the parties apart) and
   off the seat (to know which function's duties apply) — never off one
   alone.

4. **A seat is occupant-agnostic by construction.** `SessionRole::Operator`
   is "the deciding seat for this session"; the doc on the enum states it is
   "occupant-agnostic (a human or an AI may hold `Operator`)." Enfranchising
   an agent to decide gates is therefore *granting the `GateDecider`
   capability to an `AutonoeticAgent` principal*, not creating a new seat —
   a parameter change, not an architectural revolution.

## Worked example: deciding a gate (P-2.20)

A good way to see all three axes move together is a gate decision:

```
principal : AutonoeticAgent  "agent:auditor.default"
seat      : Operator         (the deciding function for this gate act)
capability: GateDecider { kinds: ["approval", "escalation"] }   ← P-2.20
```

- The agent is *allowed* to decide because it holds the `GateDecider`
  capability, checked at `policy.rs::can_decide_gate` →
  `PolicyDecision::allow("P-2.20")` (`policy.rs:865`). Without the
  capability, the same principal in the same seat is denied with rule
  `P-2.20`.
- The decision is *attributed* as `(Principal::agent("auditor.default"),
  SessionRole::Operator)` by `decider_seat`
  (`runtime/session_timeline.rs:698`) — so the causal chain records both
  *who* and *in what function*.
- The *duty* (O-1: record a reason; O-2: non-repudiable attribution) binds
  the act regardless of the principal's kind. A `Human` operator and an
  `AutonoeticAgent` auditor, each deciding a gate they are authorized to
  decide, owe the same motivation and receive the same Ri-0.15
  `DecisionContext`.

`GateDecider` is therefore correctly a **capability**. The phrase "the
decider is a seat" is loose shorthand for "the decider is a *function*
exercised from the `Operator` seat, authorized by the `GateDecider`
capability."

## The trap this vocabulary prevents

Before these were separated, `decider_seat` had no arm for
`PrincipalKind::ServedUser` and silently rewrote a served-user decision as
`PrincipalKind::Script` — the misattribution the `ServedUser` variant
existed to prevent. The fix was not to add a seat; it was to give the
principal kind its own attribution arm, keeping the kind distinct from both
`Human` (the operator) and `Script` (mechanical). The general lesson: **a
new principal kind must be handled at every site that pattern-matches on
kind, or it will be silently bucketed into whatever the `_` arm defaults
to.** Exhaustive matches (no `_`) force this; wildcard matches let it slip.

## Code map

| Concept | Type | Where |
|---|---|---|
| Principal (kind + id) | `Principal`, `PrincipalKind` | `autonoetic-types/src/principal.rs` |
| Seat / role | `SessionRole` | `autonoetic-types/src/session_timeline.rs` |
| Capability | `Capability` | `autonoetic-types/src/capability.rs` |
| Decider → (principal, seat) | `decider_seat` | `autonoetic-gateway/src/runtime/session_timeline.rs` |
| Agent-id → seat | `derive_role` | `autonoetic-gateway/src/runtime/session_timeline.rs` |
| Capability check (gates) | `can_decide_gate` | `autonoetic-gateway/src/policy.rs` |
| Decider kind from `decided_by` string | `decider_principal_kind` | `autonoetic-types/src/principal.rs` |

## See also

- `docs/concepts/philosophy.md` §3.2 — "the office is defined before the occupant."
- `docs/design/principal-model-and-symmetric-obligations.md` — the RFC that
  proposed the unified `Principal` (Part A) and authority-as-role (Part D);
  this page is the stable vocabulary those proposals assume.
- `docs/AGENTS.md` — the agent-facing capabilities reference (the
  capability axis, in operational detail).
- `docs/concepts/separation-of-powers.md` — reasoning / enforcement / decision held
  by different parties; the seat axis is how those parties are attributed.
