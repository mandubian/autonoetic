# Direct Agent-to-Agent Messaging

## Objective
To enable direct, asynchronous peer-to-peer (P2P) messaging between distinct running agent sessions, eliminating the restriction of strictly parent-child delegation.

## Delegation vs. Messaging

It is important to understand when to use `agent_message` compared to traditional task delegation (`agent_spawn` and `workflow` tools).

**1. Delegation (`agent_spawn`)**
- **Hierarchical:** Always establishes a parent-child relationship.
- **Task-oriented:** Used to completely hand off a defined unit of work because the current agent lacks the explicit capabilities to perform it (e.g., Planner delegating to Coder to execute code).
- **Structured workflow context:** The caller is resumed by gateway-owned child-state wake-up when the child changes state, and may still use `workflow_wait` or `workflow_state` for explicit inspection and artifact lookup.

**2. Messaging (`agent_message`)**
- **Peer-to-Peer:** Non-hierarchical. Messages can be sent across unrelated active sessions.
- **Signal-oriented:** Used for side-channel coordination, pinging, broadcasting state updates, or nudging an already-running session.
- **Asynchronous context:** The message is sent asynchronously and is purely injected into the target's conversation history. The sender doesn't formally wait for a structured checkpoint response, making it ideal for event-driven decoupled systems.

## Data Structures

To support both immediate 1-to-1 messaging and foresee future 1-to-N (multicast/broadcast) use cases, the messaging system is split into two tables:

1. **`agent_messages`**: Stores the canonical message payload.
2. **`agent_message_deliveries`**: Acts as an inbox for each target session, allowing a single message to span multiple recipient sessions.

```sql
CREATE TABLE IF NOT EXISTS agent_messages (
    message_id TEXT PRIMARY KEY,
    sender_session_id TEXT NOT NULL,
    sender_agent_id TEXT NOT NULL,
    target_pattern TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS agent_message_deliveries (
    message_id TEXT NOT NULL,
    target_session_id TEXT NOT NULL,
    delivered_at TEXT,
    PRIMARY KEY (message_id, target_session_id)
);
```

## Addressing and Recipient Resolution

`agent_message` takes `target_session_id`, `target_agent_id`, or both; at least
one is required. `target_session_id` wins for delivery when both are present.

- **`target_session_id`** — one session. The gateway resolves that session's
  bound agent (`session_agent_bindings`) and applies the ACL to it, so a session
  id cannot widen a scoped grant. A session id with no binding is refused
  (`target_session_not_found`) rather than delivered to unchecked.
- **`target_agent_id`** — broadcast to that role's **unfinished** sessions,
  excluding the sender's own session.

"Unfinished" is `GatewayStore::list_unfinished_sessions_for_agent`: bindings with
no `session_outcomes` row. `session_agent_bindings` is append-only — one row per
session ever bound, never deleted — so it is a historical index, not a liveness
index. A `session_outcomes` row is written unconditionally at session close, so
its **presence** marks a session as closed and its absence means the session has
not finished. Residual case: a session killed without a clean close leaves no
outcome row and stays listed; messaging it queues a delivery nothing consumes,
the same observable failure as messaging a hung session.

Result contract — treat only `ok == true && status == "delivered" &&
recipients_count > 0` as sent. Every other status leaves `recipients_count == 0`
and queues nothing:

| `status` | Meaning |
|---|---|
| `no_live_recipients` | The role is installed but has no unfinished session other than the sender's. |
| `target_agent_not_found` | No agent with that id is installed. |
| `target_agent_unavailable` | The agent is installed but its manifest could not be loaded — a broken bundle, not a missing one. |
| `target_session_finished` | The session has a terminal `session_outcomes` row. Injection happens at wake and a finished session does not wake again, so nothing is queued. |
| `target_session_not_found` | The session id has no `session_agent_bindings` row, so the gateway cannot tell which agent owns it and will not deliver unchecked. |

`exists` distinguishes the two `target_agent_*` cases (`false` for not-found,
`true` for unavailable) and is absent when no `GatewayConfig` was supplied.

Existence is resolved through the **alias registry first**, then the agents
directory. A promoted revision is installed as an `agent_aliases` row pointing at
`runtime/revisions/<rev>` and has no directory under `agents_dir`, so a
filesystem-only lookup reports a fully installed, `agent_inspect`-able agent as
missing. `agent_message` must answer the existence question the same way
`agent_list` does.

The liveness gate applies to **both** addressing modes. `target_agent_id`
enumerates unfinished sessions; `target_session_id` checks the one session it was
given. Gating only the enumerating path left the session path reporting
`delivered` for a session that had already closed — observed in a live run with
the close preceding the send by several seconds.

## Resident Sessions (who is reachable at all)

A session normally dies with its task, so for most of this subsystem's life the
set of addressable peers was close to empty: workers run to completion, and only
an orchestrator blocked on children stays around. Messaging worked; it had nobody
to talk to.

An agent opts into **residency** with `agent.resident_idle_ttl_secs` in its
SKILL.md:

```yaml
agent:
  id: "reasoning-responder"
  name: "Reasoning Responder"
  description: "..."
  resident_idle_ttl_secs: 900
```

A resident session, on finishing its task, parks in `YieldReason::Idle` instead
of terminating: an Idle checkpoint is written, a `session_residency` row records
it as addressable, and — critically — **no `session_outcomes` row is written**,
because that row is what marks a session finished for every downstream reader. An
inbound message resumes it through the normal notification pump; when it finishes
again it re-parks, refreshing the TTL. After `resident_idle_ttl_secs` without
traffic the scheduler's reaper writes the outcome row, clears the residency, and
the session is closed for good.

Residency does **not** reuse context across *new* tasks — an inbound message
continues the same session, so history accumulates. Context reuse for fresh tasks
is the separate, deferred stateful-singleton question.

### Why a dedicated table

Addressability used to be inferred, and both available signals are wrong:

- `session_agent_bindings` is append-only — every session ever bound, dead or
  alive.
- `session_outcomes` receives a row at the **first** finalize, suspended sessions
  included, so "has no outcome row" means *currently executing*, not *reachable*.
- `session_transcripts.lifecycle_state` is only ever set to `hibernated` /
  `awaiting_gate` and never cleared on close.

`session_residency` is written when a session parks and deleted when it resumes
or is reaped, so it states reachability instead of inferring it.
`GatewayStore::list_addressable_sessions_for_agent` is residency plus
still-executing sessions, and is what a broadcast resolves against.

Residency is opt-in through `resident_idle_ttl_secs`, which no reference bundle
declares — so in practice that first half is empty and everything rests on the
second.

### The liveness ledger (#1231)

"Still executing" was itself inferred from the absence of an outcome row, and
that inference was wrong in both directions a session can come back:

- A **room or root session** closes and wakes on every operator turn. The
  outcome row is written at each close and never removed, so after its first
  turn the session read as finished forever. Observed in `session-4d4c3f46`: the
  root's row was written at 13:40, and the same session went on to spawn five
  children and send a peer message of its own at 14:25 — having refused three
  inbound messages in between.
- A session **suspended at a yield point** (`WaitingForChild`,
  `ApprovalRequired`, `UserInputRequired`, `HumanEscalation`) is parked, not
  ended. Suspension writes an outcome row like any other close, so a parent
  blocked waiting for its child was unreachable by that very child.

Neither could be fixed by reinterpreting an existing signal, because every one
of them is one-way: `session_outcomes` survives close, and
`session_transcripts.status` / `.lifecycle_state` are sticky-terminal *by
design* (125485f5 — a stale, poll-driven upsert must not resurrect a closed
session).

`session_liveness` records the missing fact directly: when a session last began
executing (`last_woken_at`, written by `AgentExecutor::execute_with_history`),
and when and how it last stopped (`last_closed_at` + `resumable`, written by
`close_session` on **every** close path, where `resumable` is
`SessionCloseOutcome::is_suspended()`). A session is addressable when it has
never closed, closed as a suspension, or has woken since it last closed.

It is ordering-based rather than state-based, and is written from the lifecycle
rather than from an observability path, so a late or duplicated transcript write
cannot move it — the sticky terminal fields stay untouched and 125485f5 stays
fixed. Sessions with no ledger row predate the table and keep the original
outcome-row behaviour, so migration reclassifies nothing.

## Wakeup Mechanism

When an agent issues a message:
1. The message and its per-recipient delivery rows are inserted into SQLite, and
   a pending `AgentMessage` notification is recorded per recipient.
2. The scheduler's notification pump (`process_pending_notifications`) delivers
   each notification as `event.ingest` against the target session, which wakes
   it.
3. That ingest carries a **wake notice only** — it deliberately does not repeat
   the message body, because the body is delivered by auto-injection (below).
   The notice declares `metadata.signal_type = "agent_message"`, which lets the
   ingress pass its text through verbatim instead of wrapping it in the
   `Gateway event type: … / Message: … / Metadata: …` envelope.

Carrying the body in *both* places is what made every peer message arrive twice,
in two different formats, only one of which matched the documented block below.

## Auto-Injection

Rather than making the recipient pull from an inbox tool, delivery is
**auto-injected**. At the top of **every turn** — and once more at wake, before
`log_wake` counts the history — `AgentExecutor::drain_pending_agent_messages`
queries `agent_message_deliveries` where `target_session_id = <current_session>`
and `delivered_at IS NULL`. Each undelivered message is appended to history as
user text:

```
[Direct Message from Agent '<sender_agent_id>' (Session: <sender_session_id>)]
<message>
```

and then marked delivered, with an `agent_message`/`received` causal event.

The drain is idempotent, so a turn with nothing pending costs one indexed
`SELECT` returning zero rows.

### Why per turn and not per wake

This used to run **once**, before the turn loop. That is only reachable for a
session that is *asleep* when the message is sent. A session already inside the
loop never revisited the drain, so its delivery row sat with `delivered_at`
NULL until the session finished — and a finished session never wakes again, so
the row was stranded permanently.

The failure was silent in the worst direction: `agent_message` answered
`{"ok": true, "status": "delivered", "recipients_count": 1}`, because the send
path proves only that the recipient was unfinished at send time, never that it
would ingest. The busiest recipients — the ones actually running — were the
least reachable, and the sender was told the opposite. It also contradicted the
tool's own guidance, which promises the block arrives "at the start of your
turn".

Draining every turn makes the wake notice an *optimisation* rather than the
delivery mechanism: the pump's `event.ingest` still wakes a sleeping recipient
promptly, but a running one no longer depends on waking at all.

## Receiver-Side Consent

`Capability::AgentMessage` is a **sender-side** grant. It answers "may I address
them" and cannot express the other half — "you may not address me" — which is
the half that matters for a role whose verdict gates the sender. An inbound
message lands in the recipient's context as user text, under guidance telling it
not to discard peer traffic, so an open inbox on an evaluator is a channel for
the judged party to lobby its judge.

A manifest declares who may write to it:

```yaml
metadata:
  autonoetic:
    messaging:
      accepts_from: []          # closed to every agent principal
      # accepts_from: ["planner.*"]   # or a specific correspondent set
```

Absent ⇒ open, which is what every bundle predating the field means. The
gateway's own notices and operator-initiated traffic are not peers and are never
filtered. An agent messaging another session of its **own** role is not subject
to the check.

Enforced in `AgentMessageTool` against the *receiving* agent's manifest —
resolved alias-first, ingest-dir second (#1136) — after target resolution, so a
nonexistent or broken recipient still reports `target_agent_not_found` /
`target_agent_unavailable` rather than a consent error. Refusal is
`recipient_refuses_peer_messages`; an unreadable recipient manifest is
`recipient_consent_unverifiable`, and neither queues anything.

`MessagingPolicy` carries `deny_unknown_fields`, and
`validate_skill_frontmatter_shape` rejects a malformed block at install time.
This is deliberate: every neighbouring manifest field fails toward *less* access
when misread, but a dropped `messaging` block leaves `accepts_from` at its
`["*"]` default and publishes an inbox its author believed was closed. A typo
(`accept_from:`) is a hard error rather than a silent reopening.

### Why declared, not inferred

R-10.7 refuses a gate decider entangled with the party whose gate it decides,
reasoning from the spawn tree because a gate names its session. Messaging has no
such subject to reason from — and, decisively, the adjudicating bundles
(`sealed_evaluator.default`, `static_evaluator.default`, `outcome-grader.default`,
`security_sentinel.default`) declare **no** adjudicating capability at all. Their
authority comes from where they sit in the promotion flow, not from anything the
gateway can infer at send time, so a capability-derived rule would miss exactly
the roles that need protecting.

### Pattern matching

Shared by both sides (`patterns_match_agent_id`): a trailing `*` is a prefix
(`watchdog.*`), `*` alone matches everything, and a pattern **without** a
trailing `*` is exact. Previously every pattern was a prefix whether it asked to
be or not — `["auditor"]` reached `auditorX.malicious` and `["coder.default"]`
reached `coder.default.evil` — and a blank entry trimmed to the empty prefix,
silently meaning `*`. This mirrors the line `AuthorityOp::patterns_allow`
already draws for authority grants.

## Tool Implementation

`AgentMessageTool` (`agent_message`) is registered in the native tool registry
and gated on the `AgentMessage` capability. It enforces
`PolicyEngine::can_message_agent()` (P-11.5) against the receiving agent in both
addressing modes.

Tier: **Workflow** (`config/tools.yaml`). This matters — as `Specialized` (the
registry default for unlisted tools) the tool is filtered out of the advertised
tool list for every child session and every un-escalated root session, including
agents that declare the capability, and its prompt guidance is dropped with it.
`child_tool_tier_filter_for_manifest` already grants the Workflow tier to
manifests declaring `AgentMessage`, so Workflow is the tier that matches.
Regression guard: `agent_message_is_workflow_tier_so_declaring_agents_can_see_it`
in `runtime::prompt_budget`.

## Operator Surface

Delivery emits an `agent.peer_message` row on the **receiving** session's
canonical timeline (`peer_message_event`), attributed to the **sender** —
sender acted, receiver was affected. Payload carries `message_id`,
`sender_agent_id`, `sender_session_id`, and the redacted body.

Altitude derives to **Normal**, the same floor as `operator.message`, so peer
traffic is visible in the room without lowering the altitude filter. A sender on
a raised seat (e.g. Sentinel) lifts it further via `role_floor`. Without this
row, an injected peer message reads as anonymous user text — indistinguishable
from something the operator typed.

The wake-notice ingest emits no timeline row of its own: it carries no body, so a
row there would either duplicate `agent.peer_message` or sit beside it as a
bodyless placeholder.

The `agent_message`/`received` causal event remains the audit record; the
timeline row is the operator-facing view.
