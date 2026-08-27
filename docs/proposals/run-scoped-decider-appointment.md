# Run-Scoped Decider Appointment — "Name the Night Watch"

> **Status: Open.** Design proposal. The decider *seat* exists in the
> constitution and in code; what is missing is the act of *appointing* an
> occupant for a run. Motivating observation: the 2026-08-27 "Night Shift"
> demo (see [`launch-presentation.md`](launch-presentation.md)) parked two
> gates for a sleeping operator for hours — the exact situation an appointed
> agent-decider resolves — and no bundled agent even holds `GateDecider`.

## Problem

Unattended runs stop at every gate that needs a verdict. The operator's
prompt can say *"tonight's decider is auditor.default"* — the Night Shift
prompt did — but nothing mechanical honors it: there is no appointment
record, no routing of gates to a designated agent, and no bundled agent
holding the capability. Today, putting an agent in the decider seat for a
run means hand-editing a manifest and hand-wiring each gate — so nobody
does it, and "unattended" degrades to "runs until the first gate."

## What already exists (mechanical inventory)

- **`GateDecider` capability** (`capability.rs`, P-2.20) with
  `kinds: [approval | escalation]`; policy check `can_decide_gate`.
- **Agent-decider decision paths** in `runtime/human_gate.rs`:
  `verify_agent_decider`, `escalate_to_human` (P-2.21 — a decider that
  cannot decide escalates rather than rejects), and causal attribution
  (`agent_decider.{kind}_gate`, `decided_by` parsed back to the agent).
- **Obligations that bind the seat**: O-1 (a decider owes a motivated
  decision; terminal decisions without a non-empty reason are rejected),
  Ri-0.15 (the decider is owed decision context by the gateway).
- **Spawn-tree exclusion** (R-10.7): a decider is mechanically refused any
  gate inside its own spawn tree — self-dealing is not a policy wish.
- **Escalation plumbing**: `ScheduledAction::SessionEscalate` already
  carries an `agent_decider` marker through the dedup chain.

So the seat, the oath, and the conflict-of-interest rule are built. The
missing piece is **appointment**: a durable, operator-authorized record
saying *for this scope, gates of these kinds route to this agent*.

## The trust analysis (why not literally "by prompt")

The demo phrasing — *"tonight's decider is auditor.default"* — is a prompt,
and a prompt is untrusted text. An appointment is a power grant: whoever
controls it controls verdicts. If raw goal-message prose could seat a
decider, a prompt-injected planner could seat an accomplice. The design
therefore has two intake paths, and **neither trusts prose**:

1. **Operator-signed appointment** (the default). The appointment is a
   first-class record created by an operator act — a CLI command, or a
   `decider:` field on the plan-frame approval card, so the operator
   appoints *while approving the overnight envelope*: "approve the plan,
   name the night watch" in one gesture.
2. **Prompt-nominated, operator-confirmed** (phase 3). The planner may
   *parse* the nomination from the goal message and file an **appointment
   request** — which is itself a gate the operator approves once. Prose
   becomes a verified act without ever being trusted: the LLM proposes,
   the human seats.

## Design

### Appointment record

Stored in gateway.db (`decider_appointments`), causal-chained
(`decider.appointed` / `decider.revoked`):

| Field | Semantics |
|---|---|
| `decider_agent` | Must hold `GateDecider` with matching `kinds` — appointment never widens capabilities |
| `kinds` | `approval`, `escalation`, or both (mirrors the capability) |
| `scope` | A root session (a run), not a standing global grant |
| `risk_ceiling` | Optional: only gates at or below this risk class route to the agent; anything above parks for the operator |
| `advice_only` | If true, the agent's verdict is *recorded* but the gate still parks for the human (see advisory mode) |
| `expiry` | Timestamp or gate count; an appointment that never expires is a standing grant and should look like one |
| `appointed_by` | The operator principal; non-repudiable like every other power act |

### Routing

When a gate opens inside an appointed scope and matches `kinds` +
`risk_ceiling`: the gateway spawns (or wakes) the decider agent with the
gate card as turn context (Ri-0.15 made literal), bounded dwell. The
decider's verdict flows through the existing agent-decider paths — O-1
motivation enforced, `agent_decider.*` attribution, R-10.7 exclusion.
**Fallback is graduated, never silent:** decider session fails, times out,
or escalates (P-2.21) → the gate parks for the human operator exactly as
today. An absent or dead night watch degrades to the status quo, never to
auto-approval.

### Advisory mode first (advisory-before-binding, philosophy §4.4)

`advice_only: true` appointments route the gate to the agent *and* keep it
parked for the human. Both verdicts are recorded. The agreement rate
between agent verdicts and eventual human decisions is computable from the
ledger — standing *computed, never asserted* (§3.2) — and becomes the
calibration evidence for later binding appointments. This is the same
staging the Sentinel follows (Ri-0.16): judgment layers earn authority from
evidence, not from assertion.

### Visibility and revocation

- The per-turn attestation (P-6.23) names the seat occupant for pending
  gates in scope — every agent knows *who* will decide, as a verified fact.
- `autonoetic gateway deciders list/revoke` — revocation is one command,
  causal-logged, takes effect on the next gate (in-flight verdicts already
  attributed stay attributed).
- Appointments appear in `gateway pending` alongside the gates themselves.

## Constitutional mapping

| Clause | Role |
|---|---|
| P-2.20 | The capability the appointee must already hold |
| P-2.21 | The decider's own escape hatch (escalate, don't guess) |
| O-1 | Every verdict motivated — human and agent alike |
| Ri-0.15 | The gateway owes the decider decision context |
| R-10.7 | No deciding inside your own spawn tree |
| §3.2 | Office before occupant — enfranchisement as parameter change, exactly this |
| §4.4 | Advisory before binding — `advice_only` is the calibration stage |

## Phases

1. **Operator-signed appointment**: record, CLI, routing, fallback,
   attestation visibility. Ship one bundled agent holding
   `GateDecider { kinds: [approval] }` (a `nightwatch.default` — currently
   *no* bundled agent holds the capability).
2. **Advisory mode + calibration tally**: `advice_only`, agreement-rate
   computed into the civic record.
3. **Prompt-nominated intake**: planner parses the nomination, files the
   appointment request as an operator gate.
4. **Standing-based eligibility** (direction): binding appointments require
   measured agreement above a threshold — itself constitutional, never a
   config knob (§3.2's anti-gerrymandering rule).

## Open questions

1. **Risk-ceiling vocabulary.** Reuse the existing approval risk classes or
   define gate "altitude"? A decider for network-host gates but not
   capability-delta gates is the obvious first cut.
2. **The decider's own gates.** If the night watch needs an approval to
   reach a verdict (read an artifact, fetch a page), who decides *that*
   gate? Probably: never the decider itself (R-10.7 already blocks
   same-tree cases); operator-parked, or a second appointee.
3. **Panels.** Multi-decider appointment (2-of-3) is the voting problem in
   miniature — deliberately out of scope until §U served-party rights are
   entrenched (the §3.3 sequencing constraint).
4. **Dwell-time parity.** Human high-risk decisions carry dwell times
   (R++4); agent verdicts should carry the same, or faster with the record
   showing it.

## Why this is the next demo beat

The Night Shift showed gates parked overnight. With phase 1, the same run
reads: the plan approval card names `nightwatch.default`; both observed
gates (artifact_exec → stooq.com, curl probes → stooq.com/pl) are decided
at 3am with motivated verdicts on the chain; the morning review is
*reading two rulings* instead of *making two decisions*. The constitution's
most distinctive claim — the office doesn't care who sits in it — stops
being prose.
