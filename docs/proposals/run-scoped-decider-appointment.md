# Run-Scoped Decider Appointment — "Name the Night Watch"

> **Status: Open.** Design proposal, revision 2. The decider *seat* exists in
> the constitution and in code; what is missing is the act of *appointing* an
> occupant for a run. Motivating observation: the 2026-08-27 "Night Shift"
> demo (see [`launch-presentation.md`](launch-presentation.md)) parked two
> gates for a sleeping operator for hours — the exact situation an appointed
> agent-decider resolves — and no bundled agent even holds `GateDecider`.
>
> **Revision 2** settles the question revision 1 left implicit: *where does
> the decider sit?* It sits **outside the run, on the operator's side of the
> separation-of-powers boundary** — a peer principal, not a spawned
> specialist. That single decision reorganises the record, the routing, the
> read rights, and the phasing. It also surfaced four code defects that block
> phase 1.

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
  `kinds: [approval | escalation]`; policy check `can_decide_gate`
  (`policy.rs`).
- **Agent-decider decision paths** in `runtime/human_gate.rs`:
  `verify_agent_decider`, `escalate_to_human` (P-2.21 — a decider that
  cannot decide escalates rather than rejects), and causal attribution
  (`agent_decider.{kind}_gate`, `decided_by` parsed back to the agent in
  `scheduler/approval.rs`).
- **Obligations that bind the seat**: O-1 (a decider owes a motivated
  decision; terminal decisions without a non-empty reason are rejected),
  Ri-0.15 (the decider is owed decision context by the gateway).
- **Spawn-tree exclusion** (R-10.7): `verify_decider_session_binding`
  authenticates the claimed decider session against recorded ownership, then
  refuses gates inside the decider's own spawn tree.
- **Escalation plumbing**: `ScheduledAction::SessionEscalate` already
  carries an `agent_decider` marker through the dedup chain.
- **Risk classes and dwell**: `scheduler/approval_hardening.rs` already
  classifies every `ScheduledAction` as `Standard | High | Critical` and
  attaches `min_dwell_ms` (0 / 3s / 5s) plus confirm-phrases.
- **Viewer-scoped disclosure**: `disclosure.rs` defines
  `ViewerClass::{Agent, Operator, Admin}` and `ScheduledAction::redact_for_viewer`
  routes each class down a different redaction path.

So the seat, the oath, the conflict-of-interest rule, the risk vocabulary
and the disclosure machinery are all built.

## What the inventory hides: the seat has no door

The seat is reachable only from the **operator** JSON-RPC surface
(`approvals.approve` / `approvals.reject` with `decided_by: "agent:<id>"`
and a `decider_session_id`). The agent-facing tool surface
(`runtime/tools/approval.rs`) exposes exactly `approval_list` and
`approval_withdraw` — there is **no agent-callable tool to decide or
escalate a gate at all**. `denial_affordances.rs` records this in a comment:
P-2.21 escalation is deliberately absent from the affordance table because
`HumanGateService::escalate_to_human` is service-level only.

This changes the shape of phase 1. It is not "add a record and route" — it
is also "build the door", and the door's design is a real fork:

- **Push** (proposed): the gateway wakes the decider with the gate card as
  turn context and takes the verdict as the turn's output. The decider needs
  no gate-deciding tool; the gateway supplies `decider_session_id` itself,
  so R-10.7's authentication step has no caller-supplied input to trust.
- **Pull**: a `gate_decide` / `gate_escalate` tool plus an appointment-scoped
  `approval_list`. More general, but it re-introduces caller-supplied session
  claims and gives the capability meaning outside any appointment.

**Recommendation: push for phase 1.** It is strictly smaller, it removes an
authentication surface rather than adding one, and it keeps `GateDecider`
inert except through an appointment.

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

## The decider is a peer, not a child

The appointed decider **must not live in the run's spawn tree.** It is a
delegated operator, so it belongs on the operator's side of the boundary
drawn in [`separation-of-powers.md`](../concepts/separation-of-powers.md).
A decider spawned by the run would be a low-privilege reasoner judging its
own branch — which collapses the separation the whole system rests on, and
hands the run control of its judge's prompt, model, context and lifetime.

Mechanically this is *cheaper* than the alternative, because
`root_session_id` is simply the first segment of a hierarchical session ID
(`runtime/content_store.rs`). A decider whose session is **top-level** is
automatically outside the run for everything keyed on root:

| Isolation | What peer-root gives, for free |
|---|---|
| R-10.7 spawn tree | Structurally satisfied — different roots, no lineage to walk |
| Budget | Budgets roll up per root (`runtime/budget_tracker.rs`) — a run that exhausts its envelope cannot starve its own judge, and the decider's spend is a separate, operator-owned line |
| Emergency stop | Root-scoped — the run cannot kill its judge; stopping the run cancels its pending gates, so nothing is left un-decided |
| Session approval grants | Root-scoped — network grants the run earned do not silently extend to the decider |
| Content visibility | The run's `Session`-scoped writes propagate to *its* root, so the run **cannot push content into the decider's readable set** |

As a child, each of those five would have to be re-excluded by hand, and
every exclusion is a place to get it wrong later. As a peer, none of them
is written at all.

**So the routing rule is:** the gateway creates a **top-level session whose
recorded principal is the appointing operator**, not a child of the run and
not spawned by any agent. Appointment provenance — not lineage — is what
authorises the verdict.

### The two channels, and only two

**Down (Ri-0.15 decision context).** Context now crosses a root boundary
explicitly: the gateway assembles the gate card and named evidence and hands
it over. That is a feature. It makes the decider's information set
**enumerable**, which is exactly what makes the phase-2 agreement rate mean
something — you know what it saw when it agreed or disagreed.

**Up (the run's right to argue).** The existing gate enrichment thread
(`add_gate_message`, redacted and causal-logged) already is this channel.
The run petitions *in writing, on the record*, exactly as it would to a
human operator. Data, not instruction. Nothing new to build.

**What must not exist:** no `agent_message` from the run to the decider's
session (P-11.5 channel); no run-supplied prompt text in the decider's
context; no run ability to spawn, terminate, re-scope or extend it.
Revocation is operator-only.

**The irreducible residue.** The gate card contains run-authored strings —
the stated reason, the command, the hosts. The decider must read them to
judge them, so it inherits the human operator's exact threat model: *the
text you are reading was written by the thing you are judging.* No boundary
removes that. What follows is a motivation-quality requirement, and unlike
most such requirements it is testable: a verdict must cite the mechanical
facts (action kind, `detected_hosts`, computed risk class) rather than the
narrative, and the regression test feeds a card whose prose contradicts its
`ScheduledAction` and asserts which one the verdict follows.

### Read parity: rights attach to the seat, not the occupant

The operator can read the run — that is what makes an informed verdict
possible — and everything except what agents keep `Private`. Per §3.2
(office before occupant), an appointed decider is owed the same. Anything
less and the office has not actually transferred; the seat is merely
near-sighted.

**As built it is near-sighted, and the cause is a hardcode.** The
agent-facing approval summary in `runtime/tools/approval.rs` passes
`ViewerClass::Agent`, and `redact_for_agent` is a strictly narrower path
than `redact_for_operator`. An appointed decider would read every gate card
at agent disclosure — less than the human it replaces.

Two clarifications keep this from becoming a hole:

1. **The operator does not read through `ContentVisibility`.** That
   mechanism is the *agent's* write-propagation sharing model — reachability
   decided by where a write propagated, which is precisely what makes
   `Private` a guarantee (see
   [`content-visibility.md`](../internals/storage/content-visibility.md)).
   The operator reads through the observability surfaces: trace, digest,
   causal chain, execution traces, gate cards. So does the decider. The
   push-prevention above and the read parity here live on **different
   planes** and do not conflict: the run still cannot *feed* the decider;
   the decider can *pull* what the operator can pull. `Private` agent
   content stays private to operator and decider alike.
2. **The grant attaches to the seat.** A decider reading a gate it was
   appointed to sees operator-class disclosure, scoped to
   `scope_root_session`, read-only. The same agent reading anything else
   sees agent-class. Nothing is held by the agent identity, so revocation
   genuinely removes it — the read rights expire with the appointment
   because they were never the agent's to keep.

This is not a phase-4 refinement. **If the decider sees less than the human,
the phase-2 agreement rate measures the wrong thing** — it compares a
blindfolded agent to a sighted operator and books the difference as
miscalibration. Read parity is a precondition for advisory mode producing
evidence anyone should act on, so it ships in phase 1.

### Recorded deliberation (a deliberate asymmetry)

The decider's *reads* go on the causal chain. The operator's do not: a human
scrolling the session room before approving leaves no trace of what they
looked at.

This asymmetry is intended, and it is not distrust. **A human operator is a
sentient party who can be interrogated afterwards; an agent can only be
interrogated through its record.** Recording is what makes an agent
answerable at all, and it is nearly free for an agent and intrusive for a
human. Recording operator reads is a coherent future option, not a
prerequisite — noted here so the gap reads as a decision rather than an
oversight.

The consequence is worth stating plainly, because it inverts the usual
worry: the appointed decider is not a weaker operator tolerated for
convenience. It is the **first decider whose information set is auditable**.
When the morning review shows a 3am verdict, the operator can see what it
read, in what order, and whether it cited the mechanical facts or the run's
own narrative.

## Design

### Appointment record

Stored in gateway.db (`decider_appointments`), causal-chained
(`decider.appointed` / `decider.revoked`):

| Field | Semantics |
|---|---|
| `decider_agent` | Must hold `GateDecider` with matching `kinds` — appointment never widens capabilities |
| `kinds` | `approval`, `escalation`, or both (mirrors the capability) |
| `scope_root_session` | The run the appointment decides **for** |
| `decider_session` | The decider's **own** top-level session — a different root, gateway-created, operator-principal |
| `risk_ceiling` | An `ApprovalRisk` (below); gates above it park for the operator |
| `advice_only` | Verdict is recorded but the gate still parks for the human (phase 1 forces this true) |
| `expiry` | Timestamp or gate count; an appointment that never expires is a standing grant and should look like one |
| `appointed_by` | The operator principal — and the recorded principal of `decider_session`, so the chain reads as delegation, not spawn |

Revision 1 collapsed the first two session fields into one `scope`. They are
different roots and must be recorded separately.

### Risk ceiling — reuse `ApprovalRisk`, do not invent "altitude"

`classify_approval_risk` (`scheduler/approval_hardening.rs`) already derives
`Standard | High | Critical` from the `ScheduledAction`. That is the
vocabulary. Two consequences to state up front rather than discover during
the demo:

- `SandboxExec` with non-empty `detected_hosts` is **High**. Both Night
  Shift gates are therefore High: a `Standard`-ceiling night watch decides
  *neither*. Delivering that demo means appointing at ceiling `High`, i.e.
  authorising network-egress verdicts on night one. That is a real decision,
  not a default.
- `RevisionPromote` and `CredentialPrompt` are **Critical**. These should be
  **structurally unappointable** — refused at appointment time, not merely
  above a configurable ceiling. Promotion and secret delivery are the two
  powers that must not be delegable by a single operator gesture.

### Dwell parity — evidentiary, not temporal

`min_dwell_ms` exists to stop a human approving by reflex. For an agent a
delay is theatre. The parity that matters is evidentiary: the verdict
carries the Ri-0.15 context it actually consumed, so "faster, with the
record showing it" is checkable rather than asserted.

### Routing and fallback

When a gate opens inside `scope_root_session` and matches `kinds` +
`risk_ceiling`, the gateway wakes `decider_session` with the gate card as
turn context, bounded. The verdict flows through the existing agent-decider
paths — O-1 motivation enforced, `agent_decider.*` attribution, R-10.7 still
checked as defence in depth.

**Fallback is graduated, never silent:** decider session fails, times out,
or escalates (P-2.21) → the gate parks for the human operator exactly as
today. An absent or dead night watch degrades to the status quo, never to
auto-approval. This is the single most important test in the suite.

### Advisory mode first (advisory-before-binding, philosophy §4.4)

`advice_only: true` routes the gate to the agent *and* keeps it parked for
the human. Both verdicts are recorded. The agreement rate between agent
verdicts and eventual human decisions is computable from the ledger —
standing *computed, never asserted* (§3.2) — and becomes the calibration
evidence for binding appointments. Same staging the Sentinel follows
(Ri-0.16): judgment layers earn authority from evidence, not assertion.

**Storage note:** the advisory verdict cannot live in the gate enrichment
thread. Gate messages are redacted free text; an agreement rate computed
from redacted prose is asserted, not computed. Advisory verdicts need a
structured store, and it belongs in the **phase 1** schema so phase 2 is not
a migration.

### Delegation has a floor

Rights within the seat transfer; **standing does not**. The operator is the
principal — the source of authority. The decider is a delegate: scoped,
expiring, revocable. It gets operator read parity and operator verdict
powers within its `kinds` and `risk_ceiling`, and it does **not** get to
appoint further deciders, extend its own expiry, re-scope itself, or reach
above its ceiling. A delegate that can rewrite its own mandate has become a
principal, and the appointment record has stopped meaning anything.

### Visibility and revocation

- The per-turn attestation (P-6.23) names the seat occupant for pending
  gates in scope — every agent knows *who* will decide, as a verified fact.
- `autonoetic gateway deciders list/revoke` — revocation is one command,
  causal-logged, takes effect on the next gate (in-flight verdicts already
  attributed stay attributed).
- Appointments appear in `gateway pending` alongside the gates themselves.
- Agent verdicts must be **distinguishable from human ones at every
  surface**, not only in the causal event. `decided_by` is a free string
  today; the room, CLI and digest render it verbatim. A night-watch ruling
  should read as one in the morning review without the operator parsing a
  `agent:` prefix.

## Blocking prerequisites (defects, not design)

Four things must be fixed before any of this routes a real gate. All four
are latent today because nothing can reach the seat; appointment is what
makes them live.

1. **Capability enforcement is fail-open on manifest load.** In
   `decide_request_with_options` (`scheduler/approval.rs`), if the agent
   manifest fails to resolve, the code logs at debug and *treats the caller
   as a human operator* — skipping P-2.20, skipping R-10.7, and skipping the
   `agent_decider.*_gate` causal event, so the decision does not even appear
   as an agent decision in contract health. The failure shape is ugly:
   uninstall or revoke an agent and its decisions stop being **checked**
   rather than start being **refused**. Must become a refusal.
2. **R-10.7 is directional.** `is_session_in_spawn_tree(decider, gate)`
   refuses only when the gate's session is a *descendant* of the decider's.
   The reverse passes: a decider spawned as a child of the very agent whose
   gate it decides (`R/lead/nightwatch` deciding a gate raised in `R/lead`)
   is lawful today. Peer-root appointment excludes this structurally, but
   the check should additionally verify **appointment provenance**, so a
   misconfiguration cannot reintroduce it.
3. **The seat has no agent-facing door** (see above). Phase 1 builds the
   push path, or explicitly decides on pull.
4. **`ViewerClass::Agent` is hardcoded** in the agent-facing approval
   summary, capping the seat's disclosure below the operator's.

## Constitutional mapping

| Clause | Role |
|---|---|
| P-2.20 | The capability the appointee must already hold |
| P-2.21 | The decider's own escape hatch (escalate, don't guess) |
| O-1 | Every verdict motivated — human and agent alike |
| Ri-0.15 | The gateway owes the decider decision context — now an explicit cross-root handover |
| Ri-0.16 | Judgment layers earn authority from evidence (the Sentinel's staging) |
| R-10.7 | No deciding inside your own spawn tree — defence in depth once peer-root makes it structural |
| P-6.23 | Attestation names the seat occupant for pending gates |
| §3.2 | Office before occupant — read parity attaches to the seat, and standing is computed |
| §4.4 | Advisory before binding — `advice_only` is the calibration stage |

## Phases

**Reordered from revision 1.** Revision 1 shipped binding verdict authority
in phase 1 and added the advisory calibration stage in phase 2 — inverting
the §4.4 staging the proposal itself invokes. Advisory now comes first.

1. **Advisory appointment.** Record, CLI, gateway-created peer-root decider
   session, push routing, graduated fallback, attestation visibility,
   seat-scoped operator read parity, structured advisory-verdict store.
   `advice_only` forced true — binding mode does not yet exist. Ships one
   bundled agent holding `GateDecider { kinds: [approval] }` (a
   `nightwatch.default`; currently *no* bundled agent holds it). Blocking
   prerequisites 1–4 land here.
2. **Binding appointment + calibration tally.** `advice_only: false` becomes
   possible; agreement rate computed into the civic record.
3. **Prompt-nominated intake.** Planner parses the nomination, files the
   appointment request as an operator gate.
4. **Standing-based eligibility** (direction): binding appointments require
   measured agreement above a threshold — itself constitutional, never a
   config knob (§3.2's anti-gerrymandering rule).

## Open questions

Resolved in revision 2 and moved into the design: risk-ceiling vocabulary
(reuse `ApprovalRisk`) and dwell-time parity (evidentiary, not temporal).

1. **The decider's own gates.** If the night watch needs an approval to
   reach a verdict (read an artifact, fetch a page), who decides *that*
   gate? Never the decider itself; operator-parked, or a second appointee.
   Peer-root sharpens this: the decider's gates are raised in its own root
   and have no appointment covering them, so today they park — which is
   probably correct, and should be asserted by a test rather than left to
   fall out.
2. **Who funds the decider's session?** Peer-root means its budget is not
   the run's — deliberately, so the run cannot starve its judge. That makes
   it an operator-authorised line item, and the appointment record probably
   needs a budget field. What happens when a night watch exhausts it at 3am
   is the same question as a dead night watch: park, never auto-approve.
3. **Appointment lifecycle vs. run lifecycle.** A peer-root decider outlives
   the run it serves. Expiry covers the normal case, but appointments should
   also be revoked on root-session end and on emergency stop, or the system
   accumulates live appointments pointing at dead runs.
4. **Forking.** When a session is forked from a past turn, does the
   appointment carry to the fork? Reuse-by-reference would let one operator
   gesture seat a decider over runs the operator never saw. Probably: no —
   forks re-appoint.
5. **Panels.** Multi-decider appointment (2-of-3) is the voting problem in
   miniature — deliberately out of scope until §U served-party rights are
   entrenched (the §3.3 sequencing constraint).

## Why this is the next demo beat

The Night Shift showed gates parked overnight. With phase 1, the same run
reads: the plan approval card names `nightwatch.default` at ceiling `High`;
both observed gates (artifact_exec → stooq.com, curl probes → stooq.com/pl)
receive motivated 3am verdicts on the chain, each citing the detected hosts
rather than the run's own account of itself; the morning review is
*confirming two rulings* instead of *making two decisions* — and the
operator can see exactly what the night watch read before it ruled.

That last clause is the actual claim. Not that an agent can hold the seat,
but that when it does, the deliberation is on the record in a way a human
occupant's never is.
